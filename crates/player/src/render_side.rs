//! The render thread (design.md "Overall Architecture", "Rendering and Skins"):
//! it owns the screen, readies it for each play on request and says when it is
//! ready, draws each play from the logic step's effects and play view at the
//! play's chart time, and draws the end screen between plays and the select
//! screen ([`crate::menu_screen`]) on song selection. The end screen's page
//! goes through a slot a newer page replaces ([`RenderLink::show_end`]), so
//! however far the render thread falls behind, one page waits for it and
//! it holds one, the graphs of each shared with the result screen's cache;
//! leaving the end screen empties the slot ([`RenderLink::clear_end`]), and
//! a signal whose page went with it draws nothing.
//! With selection the
//! chart is not known at the start: each chosen chart is loaded on request
//! ([`RenderMsg::Load`]) and stays until the next. From a chart's choice
//! to its countdown the play screen is drawn in its loading state, through
//! the chart's load and the play's preparation, whose answers come between
//! its frames: the skin's loading frame ([`Screen::loading`]) with the
//! load's lines over it. The lines go through a slot as the end screen's
//! page does, where newer lines replace those not yet taken and at most
//! one signal waits in the channel ([`LoadingSink`]), so a slow render
//! thread never falls behind on the load's progress. A window target
//! paces frames on the swap chain's waitable object; an offscreen one at about
//! 60 Hz at most, each frame waiting for the GPU to finish the one before
//! or skipped while it has not.
//!
//! The window's physical size reaches the render thread through a slot
//! that holds the newest only ([`Extent`]): the window thread publishes
//! every resize and DPI change there and wakes the render thread with at
//! most one message waiting, and the render thread applies the newest
//! between frames, again to a screen built anew. While the window is
//! minimized (a zero side) frames go on being built, unpaced and
//! unpresented, and messages and handshakes are answered as ever; the
//! newest nonzero size is applied when it comes back.
//!
//! A frame that fails (the surface was lost) drops the screen and tells
//! the game thread, which ends the play; the next play's preparation builds
//! the screen again, and if that fails the run ends with the error. A frame
//! the skin refuses ([`Screen::build`]) also ends the play, but keeps the
//! screen and its resources: nothing is drawn or built again for that play,
//! and the next play's preparation readies the same screen afresh.
//!
//! An attempt's preparation is a request the render thread answers
//! between frames, numbered by the link: a screen may take several polls
//! to answer ([`Screen::prepare_attempt`]), and loading frames go on
//! meanwhile. What a ready preparation staged is installed only by the
//! commit of that request ([`RenderLink::commit_attempt`]), after the
//! game's last checks; a cancelled, refused, timed out or superseded
//! request is discarded, so an answer that comes late never installs.
//! The declarative bundle committed is kept apart from the screen: a
//! screen built again after a failure restores exactly it from memory
//! ([`Screen::restore`]).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bms_model::{Chart, TimeSource, TimeUs};
use eval::{JudgeEvent, Section};
use game::{Chain, Preparation, PreparationRefusal, RenderSide, ResultGraphs};
use input::QpcClock;
use render::measure::{FrameStatistics, Presentation};
use render::{
    AttemptInput, CommitError, FrameInput, FrameTiming, PreparedToken, RenderError, SkinBundle,
};
use skin_model::{BuildError, PlayContext};

use crate::attempt_context::AttemptContext;
use crate::loading_text::LoadingUpdate;
use crate::measure::{Drawn, Figures, Measure, PlayMeasure};
use crate::menu_screen::{MenuPage, SPLASH};
use crate::tuning::LiveTuning;

/// What draws: the renderer, or a double in tests.
pub trait Screen: Send {
    /// Prepares `chart` and resets the play state; with a practice round's
    /// `section`, only its targets are drawn.
    ///
    /// # Errors
    ///
    /// Why it cannot draw the chart.
    fn load(&mut self, chart: &Chart, section: Option<&Section>) -> Result<(), RenderError>;
    /// Prepares the attempt `a` (its chart, its context and its labels,
    /// borrowed for this call only) for preparation request `request`,
    /// staging `skin`'s installation when one is chosen, else readying the
    /// skin in use; nothing it stages draws before its
    /// [`commit`](Self::commit). `None` while the answer is not ready: it
    /// is asked again between frames with the same attempt, until it
    /// answers or the request is discarded. A skin that cannot draw the
    /// attempt refuses it, and the screen stays usable for the menus. The
    /// default prepares with [`load`](Self::load), the section's targets
    /// only for a section, and takes its skin's refusal as the attempt's;
    /// it refuses a declarative skin, which it cannot draw.
    ///
    /// # Errors
    ///
    /// Why the screen or its device cannot draw at all: the screen must be
    /// built again.
    fn prepare_attempt(
        &mut self,
        a: &AttemptInput<'_>,
        skin: Option<&Arc<SkinBundle>>,
        request: u64,
    ) -> Option<Result<Preparation, RenderError>> {
        let _ = request;
        if skin.is_some() {
            return Some(Ok(refusal("this screen draws no declarative skin")));
        }
        Some(match self.load(a.chart, a.section()) {
            Ok(()) => Ok(Preparation::Ready),
            Err(RenderError::Skin(e)) => Ok(refusal(e.reason)),
            Err(e) => Err(e),
        })
    }
    /// Identifies the device this screen draws with: a screen built again
    /// has another. The default is 0.
    fn device(&self) -> u64 {
        0
    }
    /// Installs what the preparation of `token` staged, whole, between
    /// frames; with nothing staged (the skin in use was readied in place)
    /// only the token's device is checked. The default installs nothing.
    ///
    /// # Errors
    ///
    /// Why it was not installed: nothing changes.
    fn commit(&mut self, token: PreparedToken) -> Result<(), CommitError> {
        if token.device == self.device() {
            Ok(())
        } else {
            Err(CommitError::Device)
        }
    }
    /// Drops what a preparation staged and was not committed. The default
    /// has nothing to drop.
    fn discard(&mut self) {}
    /// Installs the committed `bundle` on a screen built again after a
    /// failure, from memory, with no attempt prepared. The default
    /// installs nothing.
    ///
    /// # Errors
    ///
    /// Why it could not be installed: the screen must be built again.
    fn restore(&mut self, bundle: &Arc<SkinBundle>) -> Result<(), RenderError> {
        let _ = bundle;
        Ok(())
    }
    /// Waits until a frame may start; `false` on a timeout. Returns at
    /// once without a frame latency waitable object.
    fn wait(&self) -> bool;
    /// Whether [`wait`](Self::wait) paces frames.
    fn paced(&self) -> bool;
    /// Builds one play frame from `f`: the app's own work, which calls
    /// nothing in wgpu. [`draw`](Self::draw) follows a frame built.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame, which allocates nothing: the
    /// attempt stops, nothing of it is drawn, and no frame of it is built
    /// again; the screen stays usable, and the next attempt's preparation
    /// builds again.
    fn build(&mut self, f: &FrameInput<'_>) -> Result<(), BuildError>;
    /// Draws the play frame [`build`](Self::build) built;
    /// [`FrameTiming::presented`] is `false` when it was not presented (the
    /// swap chain image timed out or the window is occluded).
    ///
    /// # Errors
    ///
    /// Why the frame failed: the screen must be built again.
    fn draw(&mut self) -> Result<FrameTiming, RenderError>;
    /// The swap chain's latest frame statistics, raw; `None` without a
    /// swap chain or when the call failed.
    fn statistics(&self) -> Option<FrameStatistics> {
        None
    }
    /// How the swap chain presents; `None` without one.
    fn presentation(&self) -> Option<Presentation> {
        None
    }
    /// Draws lines of text, as [`draw`](Self::draw).
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw).
    fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError>;
    /// Draws the end screen's `page`, its lines and its graphs, as
    /// [`draw`](Self::draw). A screen that draws no graphs draws the lines
    /// as [`text`](Self::text).
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw).
    fn end(&mut self, page: &EndPage) -> Result<bool, RenderError> {
        let lines: Vec<&str> = page.lines.iter().map(String::as_str).collect();
        self.text(&lines)
    }
    /// Draws the select screen's `page`, laid out again when `changed`
    /// (or when this screen has not laid it out yet). A screen that
    /// draws no menus draws nothing.
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw), and why the page's text cannot be prepared.
    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        let _ = (page, changed);
        Ok(FrameTiming {
            cpu: Duration::ZERO,
            sprites: 0,
            dropped: 0,
            presented: true,
            present_count: None,
        })
    }
    /// Draws the play screen while a chart loads before the countdown:
    /// the skin's loading frame with `page`, the load's lines, over it,
    /// laid out again when `changed` (design.md "Play Lifecycle"). `update`
    /// is the same moment typed, when the lines came with it, of the same
    /// loading lifetime. The default draws the lines as a menu page.
    ///
    /// # Errors
    ///
    /// As [`menu`](Self::menu).
    fn loading(
        &mut self,
        page: &MenuPage,
        update: Option<&LoadingUpdate>,
        changed: bool,
    ) -> Result<FrameTiming, RenderError> {
        let _ = update;
        self.menu(page, changed)
    }
    /// Draws the launch screen `elapsed` after the render thread started,
    /// as [`text`](Self::text), until the game sends what to draw; `None`
    /// draws nothing instead (no launch screen).
    ///
    /// # Errors
    ///
    /// As [`menu`](Self::menu).
    fn splash(&mut self, elapsed: Duration) -> Option<Result<bool, RenderError>> {
        let _ = elapsed;
        None
    }
    /// Applies the window's newest physical size between frames: a zero
    /// side minimizes ([`minimized`](Self::minimized)), and the play state
    /// is kept. The default ignores it.
    fn resize(&mut self, width: u32, height: u32) {
        let _ = (width, height);
    }
    /// The target's size in pixels as last applied, never zero: what a
    /// play frame is projected onto ([`FrameInput::target`]). The default
    /// is the 1920×1080 layout's.
    fn target(&self) -> [u32; 2] {
        [1_920, 1_080]
    }
    /// Whether the window is minimized: frames are neither paced nor
    /// presented until a nonzero size is applied. The default is never.
    fn minimized(&self) -> bool {
        false
    }
    /// The last frame's pixels (RGBA8) and size, for an offscreen target.
    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)>;
}

/// The window's newest physical size, from the window thread to the render
/// thread: each publish replaces the size not yet applied, and wakes the
/// render thread with a [`RenderMsg::Resize`] only when none is on its
/// way, so however often the window resizes, one size and one message
/// wait.
///
/// One render thread is woken: the one whose link registered last
/// ([`RenderLink::follow`]); a link that registered before is superseded
/// and gets no later wake.
///
/// Its invariant, for each registration: while a render thread follows the
/// slot, `queued` is set only while a `Resize` sent after that
/// registration's `Follow` waits in the channel, because
/// [`RenderLink::follow`] clears it and sends its `Follow` under the lock
/// every publish sends under, and the render thread acknowledges the slot
/// it follows as it takes each `Resize`. A flag an earlier registration
/// left set (its wake sent to a thread that ended, or taken while another
/// slot was followed) is cleared by the next one; a `Resize` from a slot
/// no longer followed may clear the followed slot's flag early, which
/// costs one extra wake, never a lost one.
#[derive(Debug)]
pub struct Extent {
    /// Width in the high 32 bits, height in the low; [`UNKNOWN`] before
    /// the first publish.
    size: AtomicU64,
    /// A [`RenderMsg::Resize`] is on its way for [`size`](Self::size).
    queued: AtomicBool,
    /// Where the wake goes, once a render thread follows the slot
    /// ([`RenderLink::follow`]).
    wake: Mutex<Option<Sender<RenderMsg>>>,
}

/// No size published yet.
const UNKNOWN: u64 = u64::MAX;

impl Default for Extent {
    fn default() -> Self {
        Self {
            size: AtomicU64::new(UNKNOWN),
            queued: AtomicBool::new(false),
            wake: Mutex::new(None),
        }
    }
}

impl Extent {
    /// Publishes the window's client size in physical pixels, zero sides
    /// when minimized, and wakes the render thread unless a wake is on its
    /// way already.
    pub fn publish(&self, width: u32, height: u32) {
        let packed = (u64::from(width) << 32) | u64::from(height);
        self.size.store(packed, Ordering::Release);
        // Under the sender's lock, so a wake is marked only when it is
        // sent: before a render thread follows, none is, and the
        // [`RenderMsg::Follow`] that comes after reads the newest size.
        let wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(tx) = wake.as_ref()
            && !self.queued.swap(true, Ordering::AcqRel)
        {
            // A render thread that ended ignores it; the flag stays set
            // until the next registration clears it.
            let _ = tx.send(RenderMsg::Resize);
        }
    }

    /// Takes the wake on its way: the render thread calls it as it takes a
    /// [`RenderMsg::Resize`], before it reads the size.
    /// A publish after this sends again; one before it was seen through
    /// the flag it set, so its size is the one read next.
    fn acknowledge(&self) {
        self.queued.swap(false, Ordering::AcqRel);
    }

    /// The newest size published, if any. Reading it takes no wake, so a
    /// wake still on its way stays the only one. Allocates nothing: the
    /// render thread calls it between play frames.
    fn latest(&self) -> Option<(u32, u32)> {
        let packed = self.size.load(Ordering::Acquire);
        // The halves of a packed size.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "each half holds one u32 side"
        )]
        let size = ((packed >> 32) as u32, packed as u32);
        (packed != UNKNOWN).then_some(size)
    }
}

/// Builds the screen on the render thread, at startup and after a failure.
pub type MakeScreen = Box<dyn FnMut() -> Result<Box<dyn Screen>, RenderError> + Send>;

/// Why the screen cannot draw a play.
#[derive(Debug, thiserror::Error)]
pub enum ScreenError {
    /// The renderer could not be built or cannot draw the chart.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// The render thread could not be spawned.
    #[error("the render thread cannot start")]
    Spawn(#[source] std::io::Error),
    /// The render thread ended before it answered a preparation.
    #[error("the render thread ended")]
    Ended,
    /// The render thread ended before its screen was built.
    #[error("the render thread ended while starting")]
    EndedWhileStarting,
    /// The render thread did not answer a chart's load in time.
    #[error("the render thread did not answer")]
    NoAnswer,
    /// The screen failed after the play's attempt was prepared, before
    /// recording opened.
    #[error("the screen failed before the play opened")]
    Lost,
}

/// The end screen: its lines and the view's graphs, if it has them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndPage {
    /// The lines, the settings lines last.
    pub lines: Vec<String>,
    /// The viewed summary's graphs, shared with the result screen.
    pub graphs: Option<Arc<ResultGraphs>>,
}

/// An attempt's refusal for `reason`, at no location.
pub(crate) fn refusal(reason: impl Into<String>) -> Preparation {
    Preparation::Refused(PreparationRefusal {
        location: None,
        reason: reason.into(),
    })
}

/// What the game thread tells the render thread.
pub enum RenderMsg {
    /// Make sure the screen can draw, building it again if it failed (the
    /// device's recovery, for a play and the menus alike), and answer when
    /// it can. It prepares no attempt.
    Prepare(mpsc::SyncSender<Result<(), RenderError>>),
    /// Prepare the chart loaded last for this attempt, building the screen
    /// again first if it failed, and answer how the preparation ended: a
    /// refusal keeps the screen. Never [`Preparation::Cancelled`], which is
    /// the game thread's. It supersedes every earlier request.
    PrepareAttempt {
        /// The request's number.
        request: u64,
        /// The attempt, owned.
        attempt: Box<AttemptContext>,
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<Preparation, RenderError>>,
    },
    /// Another chart: prepare it, building the screen again if it failed,
    /// and answer when ready. Every play draws it until the next.
    Load {
        /// The chart.
        chart: Arc<Chart>,
        /// The section a practice round draws only the targets of.
        section: Option<Section>,
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<(), RenderError>>,
    },
    /// Install what the preparation of this request staged, and answer
    /// whether it was: not when the request was discarded or superseded,
    /// or its screen was built again since.
    Commit {
        /// The request.
        request: u64,
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<(), CommitError>>,
    },
    /// Drop the preparation of this request, answered or not, unless
    /// committed: it was cancelled, refused or timed out.
    Discard(u64),
    /// Prepare every later attempt with this declarative skin, or with the
    /// skin in use (tests: production draws the simple skin).
    #[cfg(test)]
    Select(Option<Arc<SkinBundle>>),
    /// Draw the select screen's page instead of a play.
    Menu(Box<MenuPage>),
    /// Draw the play screen's loading state of this loading lifetime with
    /// the lines in the slot ([`LoadingSink`]), kept through a chart's load
    /// and a preparation until something else is drawn; nothing when the
    /// lifetime has ended ([`RenderLink::clear_loading`]).
    Loading(u64),
    /// Loading has ended: stop drawing the loading state, and answer once
    /// no loading frame can be drawn any more, with whether the screen is
    /// still there ([`RenderLink::end_loading`]).
    EndLoading(mpsc::SyncSender<bool>),
    /// A play starts: chart time 0 at the measurement's `origin` on the
    /// clock.
    Play {
        /// The logic step's effects and play view.
        side: RenderSide,
        /// The gauge to show.
        gauge: usize,
        /// What its measurement needs.
        measure: Box<PlayMeasure>,
    },
    /// The play ended at `end` on the clock: answer with its measured
    /// figures, and count nothing more until the next play.
    Finish {
        /// When it ended.
        end: TimeUs,
        /// Where the figures go.
        reply: mpsc::SyncSender<Option<Figures>>,
    },
    /// The window's size changed: apply the newest in the [`Extent`]
    /// followed, between frames.
    Resize,
    /// Follow this window's size from here ([`RenderLink::follow`]).
    Follow(Arc<Extent>),
    /// Draw the end screen instead of a play: the newest page in the slot
    /// ([`RenderLink::show_end`]), or the one drawn last when an earlier
    /// signal already took this one's page or a newer one; nothing once
    /// the pages shown were cleared ([`RenderLink::clear_end`]).
    End,
    /// The run ends.
    Exit,
}

/// The inputs of every play's frames.
#[derive(Clone, Debug)]
pub struct View {
    /// Hi-speed (the visible time) and the display offset, read at every
    /// frame.
    pub tuning: Arc<LiveTuning>,
    /// The effect queue's capacity: the most effects one frame drains.
    pub effects: usize,
}

/// How long the game thread waits for a chart's load: the render thread
/// answers between frames, after the skin's uploads.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a waiting attempt's preparation looks at whether the player
/// left.
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// How long the end of the render thread is waited for: past it, the thread
/// is left to the process's exit.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the game thread waits for a play's figures. The render thread
/// answers between frames, so this bounds only a stalled render thread: a
/// slow frame, such as a software adapter drawing and the dev-tools frame
/// dump encoding an image, can take seconds, and the figures are still
/// wanted after it.
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);

/// Something that looks at frames as they are drawn, after the measurement
/// (design.md "Latency Figures"): the dev-tools frame dump. Called after every
/// frame, presented or not.
pub trait FrameSink: Send {
    /// Called after each frame.
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>);
}

/// No frame sink.
pub struct NoFrames;

impl FrameSink for NoFrames {
    fn frame(&mut self, _screen: &dyn Screen, _drawn: &Drawn<'_>) {}
}

#[derive(Debug)]
struct Shared {
    failed: AtomicBool,
    /// When the current play's first frame was drawn, on the clock.
    first_frame: AtomicI64,
    /// The end screen's slot: its newest page, and the pages left with the
    /// end screen.
    end_page: Mutex<EndSlot>,
    /// The number of the end page last presented, after the frame sink
    /// saw it (dev-tools).
    #[cfg(feature = "dev-tools")]
    end_drawn: AtomicU64,
    /// Loading frames presented (dev-tools: a slow load's checkpoints).
    #[cfg(feature = "dev-tools")]
    loading_drawn: AtomicU64,
    /// The loading state's slot.
    loading: Mutex<LoadingSlot>,
}

/// The loading state's lifetime, its newest page and the typed update
/// shown with it until the render thread takes them or the lifetime ends,
/// and whether a [`RenderMsg::Loading`] of this lifetime waits in the
/// channel. Each clear starts a new lifetime: a sink, a signal or a frame
/// of an older one does nothing.
#[derive(Debug, Default)]
struct LoadingSlot {
    lifetime: u64,
    pending: Option<TakenLoading>,
    signaled: bool,
}

/// A loading page taken from the slot, with its typed update if it had one.
type TakenLoading = (Box<MenuPage>, Option<Box<LoadingUpdate>>);

/// The end screen's newest page with its number, until the render thread
/// takes it or the end screen is left, and the number of the last page
/// shown when it was last left: no page up to it is drawn again.
#[derive(Debug, Default)]
struct EndSlot {
    page: Option<(u64, EndPage)>,
    cleared: u64,
}

impl Shared {
    fn slot(&self) -> std::sync::MutexGuard<'_, EndSlot> {
        self.end_page.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts page `n` into the slot, returning what it held.
    fn put_end(&self, n: u64, page: EndPage) -> Option<(u64, EndPage)> {
        self.slot().page.replace((n, page))
    }

    /// Leaves the end screen with page `upto` shown last: the slot is
    /// emptied, returning what it held, and no page up to `upto` is valid.
    fn clear_end(&self, upto: u64) -> Option<(u64, EndPage)> {
        let mut slot = self.slot();
        slot.cleared = slot.cleared.max(upto);
        slot.page.take()
    }

    /// Takes the waiting page, if any.
    fn take_end(&self) -> Option<(u64, EndPage)> {
        self.slot().page.take()
    }

    /// The number of the last page cleared.
    fn end_cleared(&self) -> u64 {
        self.slot().cleared
    }

    fn loading_slot(&self) -> std::sync::MutexGuard<'_, LoadingSlot> {
        self.loading.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the waiting lines of loading lifetime `lifetime` and their
    /// typed update, if it is still the current one: the signal that came
    /// for them is taken with them. A signal of an ended lifetime takes
    /// nothing.
    fn take_loading(&self, lifetime: u64) -> Option<TakenLoading> {
        let mut slot = self.loading_slot();
        if slot.lifetime != lifetime {
            return None;
        }
        slot.signaled = false;
        slot.pending.take()
    }

    /// Whether loading lifetime `lifetime` is still the current one.
    fn loading_current(&self, lifetime: u64) -> bool {
        self.loading_slot().lifetime == lifetime
    }

    /// Ends the current loading lifetime: its waiting lines are dropped,
    /// and its sinks, signals and frames do nothing from here.
    fn end_lifetime(&self) {
        let mut slot = self.loading_slot();
        slot.lifetime = slot.lifetime.wrapping_add(1);
        slot.signaled = false;
        drop(slot.pending.take());
    }
}

/// Where the loading state is shown from, on any thread (the game thread
/// and the decode pool): the newest page replaces one the render thread
/// has not taken yet, and a signal is sent only when none waits, so at most
/// one page and one signal wait whatever the render thread's pace. A sink
/// belongs to the loading lifetime current when it was made, and shows
/// nothing once that lifetime has ended.
#[derive(Clone)]
pub struct LoadingSink {
    tx: Sender<RenderMsg>,
    shared: Arc<Shared>,
    lifetime: u64,
}

impl LoadingSink {
    /// Shows `page` as the loading state, if the sink's lifetime is still
    /// the current one, without a typed update.
    #[cfg(test)]
    pub fn show(&self, page: MenuPage) {
        self.put(page, None);
    }

    /// Shows `page` as the loading state with `update`, the same moment
    /// typed, if the sink's lifetime is still the current one: the two
    /// replace a waiting pair together.
    pub fn show_typed(&self, page: MenuPage, update: LoadingUpdate) {
        self.put(page, Some(Box::new(update)));
    }

    fn put(&self, page: MenuPage, update: Option<Box<LoadingUpdate>>) {
        let mut slot = self.shared.loading_slot();
        if slot.lifetime != self.lifetime {
            return;
        }
        drop(slot.pending.replace((Box::new(page), update)));
        if !slot.signaled {
            slot.signaled = true;
            // A thread that already ended ignores it.
            let _ = self.tx.send(RenderMsg::Loading(self.lifetime));
        }
    }
}

/// How the render thread ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderEnd {
    /// It ended and was joined.
    Joined,
    /// It ended by a panic.
    Panicked,
    /// It did not end in time and was left to the process's exit.
    Stuck,
}

/// The game thread's end of the render thread.
pub struct RenderLink {
    tx: Sender<RenderMsg>,
    shared: Arc<Shared>,
    handle: JoinHandle<()>,
    /// Disconnects when the thread ends, however it ends.
    ended: Receiver<()>,
    /// How long a preparation or a chart's load is waited for.
    answer_timeout: Duration,
    /// How long the end of the thread is waited for.
    exit_timeout: Duration,
    /// How long a play's figures are waited for.
    finish_timeout: Duration,
    /// The number of the last end page shown.
    end_pages: AtomicU64,
    /// The number of the last attempt's preparation requested.
    requests: AtomicU64,
}

impl RenderLink {
    /// Sends a message; a thread that already ended ignores it.
    pub fn send(&self, msg: RenderMsg) {
        let _ = self.tx.send(msg);
    }

    /// Makes the render thread follow the window's size in `extent`: the
    /// newest size published, and every later one, is applied to the
    /// screen between frames, and to each screen built again. The slot
    /// wakes one render thread, this link's from now on: a link that
    /// followed it before gets no later wake.
    pub fn follow(&self, extent: &Arc<Extent>) {
        // Held through the `Follow`, so no `Resize` overtakes it, and a
        // flag an earlier registration left set is cleared ([`Extent`]'s
        // invariant).
        let mut wake = extent.wake.lock().unwrap_or_else(PoisonError::into_inner);
        *wake = Some(self.tx.clone());
        extent.queued.store(false, Ordering::Release);
        self.send(RenderMsg::Follow(Arc::clone(extent)));
    }

    /// Shows the end screen's `page`: it replaces a page the render thread
    /// has not taken yet, which is dropped here, so at most one waits
    /// whatever the render thread's pace.
    pub fn show_end(&self, page: EndPage) {
        let n = self.end_pages.fetch_add(1, Ordering::Relaxed) + 1;
        drop(self.shared.put_end(n, page));
        self.send(RenderMsg::End);
    }

    /// The end screen is left (a retry, selection, the practice page): a
    /// page still waiting is dropped, and no page shown so far is drawn
    /// again, by a signal still pending or by a frame of the page drawn
    /// last.
    pub fn clear_end(&self) {
        let upto = self.end_pages.load(Ordering::Relaxed);
        drop(self.shared.clear_end(upto));
    }

    /// Waits at most `timeout` until the end page shown last has been
    /// presented, and the frame sink has seen it (dev-tools: a script's
    /// `drawn` step). Whether it was.
    #[cfg(feature = "dev-tools")]
    pub fn wait_end_drawn(&self, timeout: Duration) -> bool {
        let shown = self.end_pages.load(Ordering::Relaxed);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.shared.end_drawn.load(Ordering::Acquire) >= shown {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Makes sure the screen can draw, built again if it failed, and
    /// waits until it can.
    ///
    /// # Errors
    ///
    /// Why the screen cannot be built.
    pub fn prepare(&self) -> Result<(), ScreenError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Prepare(tx));
        self.answer(&rx)
    }

    /// Prepares the chart loaded last for `attempt`, as a new request that
    /// supersedes the ones before, and waits for the answer, at most the
    /// answer's bound, looking at `cancelled` every [`CANCEL_POLL`]: once
    /// it says so, the preparation is [`Preparation::Cancelled`] without
    /// waiting further. A cancelled or timed out request is discarded, so
    /// its answer, however late, installs nothing.
    ///
    /// # Errors
    ///
    /// Why the screen or its device cannot draw at all,
    /// [`ScreenError::NoAnswer`] past the bound and [`ScreenError::Ended`]
    /// when the render thread is gone.
    pub fn prepare_attempt(
        &self,
        attempt: AttemptContext,
        cancelled: impl Fn() -> bool,
    ) -> Result<Preparation, ScreenError> {
        let request = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::PrepareAttempt {
            request,
            attempt: Box::new(attempt),
            reply,
        });
        let deadline = Instant::now() + self.answer_timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left.min(CANCEL_POLL)) {
                Ok(answer) => return Ok(answer?),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(ScreenError::Ended),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if cancelled() {
                self.send(RenderMsg::Discard(request));
                return Ok(Preparation::Cancelled);
            }
            if Instant::now() >= deadline {
                self.send(RenderMsg::Discard(request));
                return Err(ScreenError::NoAnswer);
            }
        }
    }

    /// Installs what the last attempt's preparation staged, and waits
    /// until it is, at most the answer's bound.
    ///
    /// # Errors
    ///
    /// [`ScreenError::Lost`] when it was not installed: the request was
    /// discarded or superseded, or the screen was built again since it
    /// was prepared; [`ScreenError::NoAnswer`] past the bound and
    /// [`ScreenError::Ended`] when the render thread is gone.
    pub fn commit_attempt(&self) -> Result<(), ScreenError> {
        let request = self.requests.load(Ordering::Relaxed);
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Commit { request, reply });
        match rx.recv_timeout(self.answer_timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                eprintln!("render: the attempt was not installed: {e}");
                Err(ScreenError::Lost)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ScreenError::NoAnswer),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ScreenError::Ended),
        }
    }

    /// Drops what the last attempt's preparation staged, if it was not
    /// committed; it never installs.
    pub fn discard_attempt(&self) {
        self.send(RenderMsg::Discard(self.requests.load(Ordering::Relaxed)));
    }

    /// Prepares every later attempt with the declarative `skin`, or with
    /// the skin in use for `None` (tests: production draws the simple
    /// skin).
    #[cfg(test)]
    pub fn select(&self, skin: Option<Arc<SkinBundle>>) {
        self.send(RenderMsg::Select(skin));
    }

    /// Prepares `chart` for every play from now on, and waits until it is.
    ///
    /// # Errors
    ///
    /// As [`PendingLoad::wait`].
    #[cfg(test)]
    pub fn load(&self, chart: Arc<Chart>, section: Option<Section>) -> Result<(), ScreenError> {
        self.begin_load(chart, section).wait(self)
    }

    /// Starts preparing `chart` for every play from now on; the game
    /// thread goes on with the rest of the load and waits for the answer
    /// later ([`PendingLoad::wait`]). A pending load dropped unanswered
    /// leaves the screen with the chart, which the next load replaces.
    pub fn begin_load(&self, chart: Arc<Chart>, section: Option<Section>) -> PendingLoad {
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Load {
            chart,
            section,
            reply,
        });
        PendingLoad(rx)
    }

    /// Loading frames presented so far (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn loading_drawn(&self) -> u64 {
        self.shared.loading_drawn.load(Ordering::Relaxed)
    }

    /// Where the current loading lifetime's lines are shown from, on any
    /// thread.
    pub fn loading(&self) -> LoadingSink {
        LoadingSink {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
            lifetime: self.shared.loading_slot().lifetime,
        }
    }

    /// Loading ended (selection, the practice page): its lifetime ends, so
    /// lines still waiting are dropped, a signal still pending takes
    /// nothing, and the render thread draws no further loading frame of it,
    /// checked after each frame's wait. The next lines shown begin a new
    /// lifetime's.
    pub fn clear_loading(&self) {
        self.shared.end_lifetime();
    }

    /// Loading ended before recording opens: [`clear_loading`](Self::clear_loading),
    /// then waits, at most [`LOAD_TIMEOUT`], until the render thread has
    /// answered that it draws no loading frame any more, so none is drawn
    /// after recording opens.
    ///
    /// # Errors
    ///
    /// [`ScreenError::NoAnswer`] after [`LOAD_TIMEOUT`],
    /// [`ScreenError::Ended`] when the render thread is gone, and
    /// [`ScreenError::Lost`] when its screen failed after the attempt was
    /// prepared (a loading frame failed): recording must not open.
    pub fn end_loading(&self) -> Result<(), ScreenError> {
        self.clear_loading();
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::EndLoading(reply));
        match rx.recv_timeout(self.answer_timeout) {
            Ok(true) => Ok(()),
            Ok(false) => Err(ScreenError::Lost),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ScreenError::NoAnswer),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ScreenError::Ended),
        }
    }

    /// The answer to a preparation or a load, waited for at most
    /// [`LOAD_TIMEOUT`]; a late answer goes nowhere.
    fn answer(&self, rx: &Receiver<Result<(), RenderError>>) -> Result<(), ScreenError> {
        match rx.recv_timeout(self.answer_timeout) {
            Ok(ready) => Ok(ready?),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ScreenError::NoAnswer),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ScreenError::Ended),
        }
    }

    /// Whether a frame failed since the last preparation.
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    /// The figures of the play that ended at `end`; `None` when the render
    /// thread did not answer or had no play to measure.
    pub fn finish(&self, end: TimeUs) -> Option<Figures> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Finish { end, reply });
        rx.recv_timeout(self.finish_timeout).ok().flatten()
    }

    /// When the current play's first frame was drawn.
    pub fn first_frame(&self) -> Option<TimeUs> {
        let t = self.shared.first_frame.load(Ordering::Acquire);
        (t != i64::MIN).then_some(TimeUs(t))
    }

    /// Ends the thread and waits for it at most [`EXIT_TIMEOUT`]; when it
    /// is [`RenderEnd::Stuck`], the window it draws into must stay open.
    pub fn exit(self) -> RenderEnd {
        self.send(RenderMsg::Exit);
        match self.ended.recv_timeout(self.exit_timeout) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Detached: a thread stuck in the screen's work must not
                // hold up the exit. The caller keeps the window open.
                eprintln!("render: the render thread did not end");
                RenderEnd::Stuck
            }
            _ => match self.handle.join() {
                Ok(()) => RenderEnd::Joined,
                Err(_) => RenderEnd::Panicked,
            },
        }
    }

    /// Sets how long a preparation, a chart's load or a play's figures
    /// and the end of the thread are waited for, in place of
    /// [`LOAD_TIMEOUT`], [`FINISH_TIMEOUT`] and [`EXIT_TIMEOUT`] (tests,
    /// and `--dev-render-timeout`).
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn set_timeouts(&mut self, answer: Duration, exit: Duration) {
        self.answer_timeout = answer;
        self.finish_timeout = answer;
        self.exit_timeout = exit;
    }
}

/// A chart's load the render thread was asked for ([`RenderLink::begin_load`]).
pub struct PendingLoad(Receiver<Result<(), RenderError>>);

impl PendingLoad {
    /// Waits for the answer, at most [`LOAD_TIMEOUT`] from here.
    ///
    /// # Errors
    ///
    /// Why the screen cannot be built or cannot draw the chart, and
    /// [`ScreenError::NoAnswer`] after [`LOAD_TIMEOUT`]; a late answer goes
    /// nowhere.
    pub fn wait(self, link: &RenderLink) -> Result<(), ScreenError> {
        link.answer(&self.0)
    }
}

/// Starts the render thread; `make` builds the screen on it, and the chart,
/// when there is one yet, is prepared before this returns, so every upload
/// is done before the first countdown (design.md "Display Latency and
/// Stutter"); a chart loaded later is prepared before its countdown too.
///
/// # Errors
///
/// Why the screen could not be built or could not draw the chart.
pub fn spawn(
    mut make: MakeScreen,
    chart: Option<Arc<Chart>>,
    section: Option<Section>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
) -> Result<RenderLink, ScreenError> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared {
        failed: AtomicBool::new(false),
        first_frame: AtomicI64::new(i64::MIN),
        end_page: Mutex::new(EndSlot::default()),
        #[cfg(feature = "dev-tools")]
        end_drawn: AtomicU64::new(0),
        #[cfg(feature = "dev-tools")]
        loading_drawn: AtomicU64::new(0),
        loading: Mutex::new(LoadingSlot::default()),
    });
    let thread_shared = Arc::clone(&shared);
    let (ended_tx, ended) = mpsc::sync_channel::<()>(0);
    let handle = std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            // Dropped when the thread ends, however it ends.
            let _ended = ended_tx;
            #[cfg(feature = "dev-tools")]
            crate::alloc_count::tag(crate::alloc_count::Tag::Render);
            let screen = match build(&mut make, None, chart.as_deref(), section.as_ref()) {
                Ok(screen) => {
                    let _ = ready_tx.send(Ok(()));
                    screen
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let mut r = Run {
                screen: Some(screen),
                started: Instant::now(),
                make,
                chart,
                section,
                clock,
                view,
                measure,
                frames,
                shared: thread_shared,
                end_page: (0, EndPage::default()),
                granted: false,
                extent: None,
                applied: None,
                selected: None,
                committed: None,
                attempt: None,
                prepared: None,
                #[cfg(feature = "dev-tools")]
                menu_stats: crate::dev::MenuStats::default(),
                #[cfg(feature = "dev-tools")]
                loading_stats: crate::dev::MenuStats::default(),
            };
            r.run(&rx);
        })
        .map_err(ScreenError::Spawn)?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(RenderLink {
            tx,
            shared,
            handle,
            ended,
            answer_timeout: LOAD_TIMEOUT,
            exit_timeout: EXIT_TIMEOUT,
            finish_timeout: FINISH_TIMEOUT,
            end_pages: AtomicU64::new(0),
            requests: AtomicU64::new(0),
        }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e.into())
        }
        Err(_) => Err(ScreenError::EndedWhileStarting),
    }
}

/// What a preparation or a load leaves drawn: the splash stays until what
/// comes next replaces it, since a hidden window shows on a presented
/// frame and a play opens only once the window is shown, and so does the
/// loading state; anything else stops.
fn after_preparing(mode: Mode) -> Mode {
    match mode {
        Mode::Splash { pending } => Mode::Splash { pending },
        // The loading state stays through the chart's load and the play's
        // preparation.
        loading @ Mode::Menu {
            loading: Some(_), ..
        } => loading,
        _ => Mode::Idle,
    }
}

/// A screen made afresh, with the `committed` bundle restored and `chart`
/// loaded.
fn build(
    make: &mut MakeScreen,
    committed: Option<&Arc<SkinBundle>>,
    chart: Option<&Chart>,
    section: Option<&Section>,
) -> Result<Box<dyn Screen>, RenderError> {
    let mut s = make()?;
    if let Some(bundle) = committed {
        s.restore(bundle)?;
    }
    if let Some(chart) = chart {
        s.load(chart, section)?;
    }
    Ok(s)
}

enum Mode {
    Idle,
    /// The launch screen, until the game sends what to draw; a select
    /// page sent before it ends ([`SPLASH`]) waits in `pending`.
    Splash {
        pending: Option<Box<MenuPage>>,
    },
    Play {
        origin: TimeUs,
        side: RenderSide,
        gauge: usize,
        first: bool,
    },
    /// The end screen: [`Run::end_page`].
    End,
    /// The select screen, or with `loading` the play screen's loading
    /// state of that loading lifetime, its lines and their typed `update`;
    /// `changed` until the page is laid out.
    Menu {
        page: Box<MenuPage>,
        update: Option<Box<LoadingUpdate>>,
        changed: bool,
        loading: Option<u64>,
    },
}

/// An attempt's preparation the screen has not answered yet.
struct Preparing {
    request: u64,
    attempt: Box<AttemptContext>,
    /// The declarative skin it stages, if one is chosen.
    skin: Option<Arc<SkinBundle>>,
    reply: mpsc::SyncSender<Result<Preparation, RenderError>>,
}

/// An attempt's preparation answered ready: its token and the declarative
/// skin it staged, if any.
struct Prepared {
    token: PreparedToken,
    skin: Option<Arc<SkinBundle>>,
}

struct Run {
    screen: Option<Box<dyn Screen>>,
    /// When the render thread started: the launch screen's time runs from it.
    started: Instant,
    make: MakeScreen,
    chart: Option<Arc<Chart>>,
    /// The section of the chart a practice round draws only the targets of.
    section: Option<Section>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
    shared: Arc<Shared>,
    /// The end screen's page last taken from the slot, and its number.
    end_page: (u64, EndPage),
    /// The screen's wait granted a frame that no draw has taken yet: the
    /// end screen was left or nothing was to be drawn. Only a presented
    /// frame gives the grant back (a swap chain's waitable object, one
    /// frame of latency), so the next frame takes it instead of waiting
    /// again, which would never end; until a new screen is built.
    granted: bool,
    /// The window's size followed, if any.
    extent: Option<Arc<Extent>>,
    /// The size last applied to the screen; `None` until one is, and for
    /// a screen built again.
    applied: Option<(u32, u32)>,
    /// The declarative skin later attempts are prepared with; `None`
    /// prepares the skin in use (tests choose one).
    selected: Option<Arc<SkinBundle>>,
    /// The declarative bundle committed last, kept apart from the screen:
    /// a screen built again restores exactly it.
    committed: Option<Arc<SkinBundle>>,
    /// The attempt's preparation not yet answered.
    attempt: Option<Preparing>,
    /// The attempt's preparation answered ready and not yet committed.
    prepared: Option<Prepared>,
    /// The select screen's frames (dev-tools).
    #[cfg(feature = "dev-tools")]
    menu_stats: crate::dev::MenuStats,
    /// The loading state's frames (dev-tools).
    #[cfg(feature = "dev-tools")]
    loading_stats: crate::dev::MenuStats,
}

impl Run {
    fn run(&mut self, rx: &Receiver<RenderMsg>) {
        let mut mode = Mode::Splash { pending: None };
        let mut effects: Vec<JudgeEvent> = Vec::with_capacity(self.view.effects);
        loop {
            let idle = matches!(mode, Mode::Idle) || self.screen.is_none();
            let msg = if idle && self.attempt.is_some() {
                // A preparation not yet answered is asked again between
                // messages.
                rx.recv_timeout(CANCEL_POLL).map_err(|e| match e {
                    mpsc::RecvTimeoutError::Timeout => TryRecvError::Empty,
                    mpsc::RecvTimeoutError::Disconnected => TryRecvError::Disconnected,
                })
            } else if idle {
                rx.recv().map_err(|_| TryRecvError::Disconnected)
            } else {
                rx.try_recv()
            };
            // The select screen is left: its frames are reported; and the
            // loading state's, once it is left.
            #[cfg(feature = "dev-tools")]
            if msg.is_ok() && !matches!(msg, Ok(RenderMsg::Menu(_))) {
                self.menu_stats.report("select");
            }
            #[cfg(feature = "dev-tools")]
            if msg.is_ok()
                && !matches!(
                    msg,
                    Ok(RenderMsg::Loading(_)
                        | RenderMsg::Load { .. }
                        | RenderMsg::Prepare(_)
                        | RenderMsg::PrepareAttempt { .. }
                        | RenderMsg::Commit { .. }
                        | RenderMsg::Discard(_))
                )
            {
                self.loading_stats.report("loading");
            }
            match msg {
                Ok(RenderMsg::Load {
                    chart,
                    section,
                    reply,
                }) => {
                    // What the screen draws of the chart (dev-tools): a
                    // section's targets only.
                    #[cfg(feature = "dev-tools")]
                    {
                        let notes = || chart.lanes.iter().flatten();
                        match &section {
                            Some(s) => println!(
                                "dev: render: a section's {} targets of {} notes are drawn",
                                notes().filter(|n| s.is_target(n)).count(),
                                notes().count()
                            ),
                            None => {
                                println!("dev: render: all {} notes are drawn", notes().count())
                            }
                        }
                    }
                    self.chart = Some(chart);
                    self.section = section;
                    let ready = self.prepare();
                    // A chart the screen cannot draw is refused, and the
                    // screen it dropped is built again without it, for the
                    // select screen; if that fails too, the failure is
                    // reported and the game builds it through a preparation.
                    if ready.is_err() {
                        self.chart = None;
                        self.section = None;
                        if self.prepare().is_err() {
                            self.shared.failed.store(true, Ordering::Release);
                        }
                    }
                    let _ = reply.send(ready);
                    mode = after_preparing(mode);
                    continue;
                }
                Ok(RenderMsg::Menu(page)) => {
                    mode = match mode {
                        // The launch screen shows out its time.
                        Mode::Splash { .. } if self.started.elapsed() < SPLASH => Mode::Splash {
                            pending: Some(page),
                        },
                        _ => Mode::Menu {
                            page,
                            update: None,
                            changed: true,
                            loading: None,
                        },
                    };
                }
                Ok(RenderMsg::Loading(lifetime)) => {
                    // Lines the slot no longer holds (taken, or their
                    // lifetime ended) change nothing.
                    if let Some((page, update)) = self.shared.take_loading(lifetime) {
                        mode = Mode::Menu {
                            page,
                            update,
                            changed: true,
                            loading: Some(lifetime),
                        };
                    }
                }
                Ok(RenderMsg::EndLoading(reply)) => {
                    // Between frames: none of the loading state follows.
                    if matches!(
                        mode,
                        Mode::Menu {
                            loading: Some(_),
                            ..
                        }
                    ) {
                        mode = Mode::Idle;
                    }
                    // Whether the screen prepared for the attempt is still
                    // there.
                    let _ = reply.send(self.screen.is_some());
                    continue;
                }
                Ok(RenderMsg::Prepare(reply)) => {
                    let ready = self.recover();
                    let _ = reply.send(ready);
                    mode = after_preparing(mode);
                    continue;
                }
                Ok(RenderMsg::PrepareAttempt {
                    request,
                    attempt,
                    reply,
                }) => {
                    self.begin_attempt(Preparing {
                        request,
                        attempt,
                        skin: self.selected.clone(),
                        reply,
                    });
                    mode = after_preparing(mode);
                    continue;
                }
                Ok(RenderMsg::Commit { request, reply }) => {
                    let _ = reply.send(self.commit(request));
                    continue;
                }
                Ok(RenderMsg::Discard(request)) => {
                    self.discard(request);
                    continue;
                }
                #[cfg(test)]
                Ok(RenderMsg::Select(skin)) => {
                    self.selected = skin;
                    continue;
                }
                Ok(RenderMsg::Play {
                    side,
                    gauge,
                    measure,
                }) => {
                    let origin = measure.origin;
                    self.measure.start(*measure);
                    self.shared.first_frame.store(i64::MIN, Ordering::Release);
                    mode = Mode::Play {
                        origin,
                        side,
                        gauge,
                        first: true,
                    };
                }
                // Without a screen (it failed), the failure stays reported
                // and nothing is drawn: the game builds the screen again
                // through a preparation, which can fail.
                // Its page, or a newer one an earlier signal took; whether
                // it may still be drawn is checked before every end frame.
                Ok(RenderMsg::End) => {
                    if let Some(page) = self.shared.take_end() {
                        self.end_page = page;
                    }
                    mode = Mode::End;
                }
                Ok(RenderMsg::Finish { end, reply }) => {
                    // Judgments still queued were never drawn in this play:
                    // N/A, not missing from the figures.
                    if let Mode::Play { side, .. } = &mut mode {
                        effects.clear();
                        side.drain_effects(|e| effects.push(*e));
                        self.measure.never_drawn(&effects);
                        effects.clear();
                    }
                    let _ = reply.send(self.measure.finish(end));
                }
                Ok(RenderMsg::Follow(extent)) => {
                    // A wake sent since the sender was set is still in
                    // the channel, after this: it is taken there.
                    self.extent = Some(extent);
                    self.applied = None;
                }
                // Applied below, as every loop applies the newest size.
                Ok(RenderMsg::Resize) => {
                    if let Some(extent) = &self.extent {
                        extent.acknowledge();
                    }
                }
                Ok(RenderMsg::Exit) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {}
            }
            self.poll_attempt();
            let Some(screen) = &mut self.screen else {
                continue;
            };
            // The window's newest size, between frames.
            if let Some(size) = self.extent.as_ref().and_then(|e| e.latest())
                && self.applied != Some(size)
            {
                screen.resize(size.0, size.1);
                self.applied = Some(size);
            }
            // Minimized, nothing is presented, so the swap chain's waitable
            // object would not be signaled: frames are paced by the clock
            // instead, and a frame granted before stays granted.
            let minimized = screen.minimized();
            if screen.paced() && !minimized {
                if !self.granted && !screen.wait() {
                    continue;
                }
                self.granted = true;
            } else {
                std::thread::sleep(Duration::from_millis(16));
            }
            // The end screen was left (its pages cleared): the page drawn
            // last goes, and nothing is drawn until what comes next, which
            // takes the frame granted.
            if matches!(mode, Mode::End) && self.end_page.0 <= self.shared.end_cleared() {
                self.end_page.1 = EndPage::default();
                mode = Mode::Idle;
                continue;
            }
            // Loading ended during the wait: its frame is not drawn, and
            // nothing is until what comes next, which takes the frame
            // granted.
            if let Mode::Menu {
                loading: Some(lifetime),
                ..
            } = mode
                && !self.shared.loading_current(lifetime)
            {
                mode = Mode::Idle;
                continue;
            }
            let loading_frame = matches!(
                mode,
                Mode::Menu {
                    loading: Some(_),
                    ..
                }
            );
            // The launch screen's time is over: the page waiting replaces it.
            let elapsed = self.started.elapsed();
            if elapsed >= SPLASH
                && let Mode::Splash { pending } = &mut mode
                && let Some(page) = pending.take()
            {
                mode = Mode::Menu {
                    page,
                    update: None,
                    changed: true,
                    loading: None,
                };
            }
            let drawn = match &mut mode {
                Mode::Idle => continue,
                Mode::Splash { pending } => {
                    let Some(drawn) = screen.splash(elapsed) else {
                        // No launch screen: the page waiting, if any, at
                        // once; else the frame granted waits for what
                        // comes next.
                        mode = pending.take().map_or(Mode::Idle, |page| Mode::Menu {
                            page,
                            update: None,
                            changed: true,
                            loading: None,
                        });
                        continue;
                    };
                    drawn.map(|presented| {
                        let timing = FrameTiming {
                            cpu: Duration::ZERO,
                            sprites: 0,
                            dropped: 0,
                            presented,
                            present_count: None,
                        };
                        (timing, None)
                    })
                }
                Mode::Play {
                    origin,
                    side,
                    gauge,
                    first,
                } => {
                    // The app region's first interval (design.md "Scope of
                    // No Allocation"): this frame's own work up to the
                    // build, before anything calls into wgpu. It ends
                    // before the build's outcome is looked at, and on any
                    // other way out.
                    #[cfg(feature = "dev-tools")]
                    let region = crate::alloc_count::AppRegion::enter();
                    effects.clear();
                    // At most what the buffer holds: the logic step can push
                    // during the drain, and the rest waits for the next
                    // frame.
                    let room = effects.capacity();
                    side.drain_effects_up_to(room, |e| effects.push(*e));
                    let now = TimeUs(self.clock.now().0 - origin.0);
                    // Hi-speed and the display offset change anytime and move
                    // the notes only: they reach the screen here and nowhere
                    // near judgment. Hi-speed changes are not in the v1 log;
                    // they arrive with the separate hi-speed stream and a
                    // format version bump (design.md "Operations unrelated to
                    // judgment go in a separate stream").
                    let t = self.view.tuning.get();
                    let overlay = self.measure.overlay();
                    let shown = overlay.iter().take_while(|l| !l.is_empty()).count();
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "`shown` counts a prefix of `overlay`"
                    )]
                    let f = FrameInput {
                        now,
                        display_offset: TimeUs(t.display_offset_us),
                        visible_time: t.visible_time(),
                        view: side.view(),
                        effects: &effects,
                        gauge: *gauge,
                        overlay: &overlay[..shown],
                        target: screen.target(),
                    };
                    let built = screen.build(&f);
                    #[cfg(feature = "dev-tools")]
                    drop(region);
                    if let Err(e) = built {
                        // The skin refused the frame: nothing of it is
                        // drawn, and no frame of this attempt is built
                        // again. The attempt stops as on the screen's
                        // failure, but the screen, its device and its
                        // resources stay, for the next preparation.
                        self.measure.failed(&effects);
                        eprintln!("render: {}", Chain(&e));
                        self.shared.failed.store(true, Ordering::Release);
                        mode = Mode::Idle;
                        continue;
                    }
                    let drawn = screen.draw().map(|timing| (timing, Some(now)));
                    // The first frame that reached the screen.
                    if matches!(&drawn, Ok((t, _)) if t.presented) && *first {
                        *first = false;
                        self.shared
                            .first_frame
                            .store(self.clock.now().0, Ordering::Release);
                    }
                    drawn
                }
                Mode::Menu {
                    page,
                    update,
                    changed,
                    ..
                } => {
                    // A screen built again after a failure lays the page
                    // out again.
                    #[cfg(feature = "dev-tools")]
                    let [_, _, _, before] = crate::alloc_count::counts();
                    let drawn = if loading_frame {
                        screen.loading(page, update.as_deref(), *changed)
                    } else {
                        screen.menu(page, *changed)
                    }
                    .map(|t| (t, None));
                    #[cfg(feature = "dev-tools")]
                    if let Ok((t, _)) = &drawn {
                        let [_, _, _, after] = crate::alloc_count::counts();
                        let stats = if loading_frame {
                            &mut self.loading_stats
                        } else {
                            &mut self.menu_stats
                        };
                        stats.frame(t.cpu, after - before, *changed);
                        if loading_frame && t.presented {
                            self.shared.loading_drawn.fetch_add(1, Ordering::Relaxed);
                        }
                        // What a refusal put on the screen, once drawn.
                        if *changed
                            && let Some(notice) = page.lines.iter().find(|l| {
                                l.style == crate::menu_screen::LineStyle::Notice
                                    && !l.text.is_empty()
                            })
                        {
                            println!("dev: drawn notice: {}", notice.text);
                        }
                    }
                    *changed = false;
                    drawn
                }
                Mode::End => screen.end(&self.end_page.1).map(|presented| {
                    let timing = FrameTiming {
                        cpu: Duration::ZERO,
                        sprites: 0,
                        dropped: 0,
                        presented,
                        present_count: None,
                    };
                    (timing, None)
                }),
            };
            if !minimized {
                self.granted = false;
            }
            match drawn {
                Ok((timing, play)) => {
                    let d = Drawn {
                        play,
                        // The measured play's: none once its figures are
                        // taken, so no frame after a `Finish` is its.
                        #[cfg(feature = "dev-tools")]
                        generation: self.measure.generation(),
                        #[cfg(feature = "dev-tools")]
                        menu: matches!(mode, Mode::Menu { .. }),
                        #[cfg(feature = "dev-tools")]
                        loading: matches!(
                            mode,
                            Mode::Menu {
                                loading: Some(_),
                                ..
                            }
                        ),
                        timing,
                        statistics: screen.statistics(),
                        presentation: screen.presentation(),
                        effects: if play.is_some() { &effects } else { &[] },
                        at: self.clock.now(),
                    };
                    // The app region's second interval: a play frame's
                    // measurement. The frame sink (the frame dump) stays
                    // outside it.
                    #[cfg(feature = "dev-tools")]
                    let region = play.is_some().then(crate::alloc_count::AppRegion::enter);
                    self.measure.frame(&d);
                    #[cfg(feature = "dev-tools")]
                    drop(region);
                    self.frames.frame(screen.as_ref(), &d);
                    // The end page drawn, for a script waiting on it
                    // (dev-tools).
                    #[cfg(feature = "dev-tools")]
                    if matches!(mode, Mode::End) && timing.presented {
                        self.shared
                            .end_drawn
                            .store(self.end_page.0, Ordering::Release);
                    }
                }
                Err(e) => {
                    // What this frame drew never reached the screen.
                    if matches!(mode, Mode::Play { .. }) {
                        self.measure.failed(&effects);
                    }
                    eprintln!("render: {}", Chain(&e));
                    self.screen = None;
                    self.shared.failed.store(true, Ordering::Release);
                    mode = Mode::Idle;
                }
            }
        }
    }

    /// Builds the screen again after a failure, without a chart: the
    /// device's recovery. A screen that can draw is kept as it is.
    fn recover(&mut self) -> Result<(), RenderError> {
        if self.screen.is_none() {
            self.granted = false;
            let mut screen = (self.make)()?;
            // The committed bundle, from memory: never read again from its
            // files, never another skin in its place.
            if let Some(bundle) = &self.committed {
                screen.restore(bundle)?;
            }
            self.screen = Some(screen);
            // The new screen takes the window's size afresh.
            self.applied = None;
        }
        self.shared.failed.store(false, Ordering::Release);
        Ok(())
    }

    /// Starts preparing the chart loaded last for `p`, recovering the
    /// screen first: whatever an earlier request staged is discarded, and
    /// `p` is asked of the screen at once and between frames until it
    /// answers ([`poll_attempt`](Self::poll_attempt)).
    fn begin_attempt(&mut self, p: Preparing) {
        self.attempt = None;
        self.drop_prepared();
        if let Err(e) = self.recover() {
            let _ = p.reply.send(Err(e));
            return;
        }
        self.attempt = Some(p);
        self.poll_attempt();
    }

    /// Asks the screen for the answer of the preparation in progress, and
    /// sends it once there is one: ready, it is kept for its commit. A
    /// refusal keeps the screen and reports no failure; an error drops it,
    /// to be built again. A screen dropped meanwhile is built again first.
    fn poll_attempt(&mut self) {
        if self.attempt.is_none() {
            return;
        }
        if self.screen.is_none()
            && let Err(e) = self.recover()
        {
            self.answer(Err(e));
            return;
        }
        if self.chart.is_none() {
            self.answer(Ok(refusal("no chart is loaded")));
            return;
        }
        let (Some(p), Some(screen), Some(chart)) =
            (&self.attempt, &mut self.screen, self.chart.as_deref())
        else {
            return;
        };
        let base = p.attempt.context();
        // A declarative skin's lanes are laid out for its own scratch side.
        let context = match &p.skin {
            Some(bundle) => PlayContext {
                scratch: bundle.scratch(),
                ..base
            },
            None => base,
        };
        let text = p.attempt.text();
        let a = AttemptInput {
            chart,
            context: &context,
            text: &text,
            target: screen.target(),
        };
        let Some(answer) = screen.prepare_attempt(&a, p.skin.as_ref(), p.request) else {
            return;
        };
        if matches!(answer, Ok(Preparation::Ready)) {
            self.prepared = Some(Prepared {
                token: PreparedToken {
                    request: p.request,
                    device: screen.device(),
                },
                skin: p.skin.clone(),
            });
        }
        if answer.is_err() {
            self.screen = None;
            self.granted = false;
        }
        self.answer(answer);
    }

    /// Sends the preparation in progress its `answer`, and ends it.
    fn answer(&mut self, answer: Result<Preparation, RenderError>) {
        if let Some(p) = self.attempt.take() {
            let _ = p.reply.send(answer);
        }
    }

    /// Installs what the preparation of `request` staged: it must be the
    /// one answered ready last, on the screen it was prepared on (its
    /// device's). Another request's commit changes nothing; one refused by
    /// the screen discards the staging. Committed, a declarative bundle is
    /// kept for a screen built again.
    fn commit(&mut self, request: u64) -> Result<(), CommitError> {
        let Some(staged) = self.prepared.as_ref().map(|p| p.token.request) else {
            return Err(CommitError::NothingStaged);
        };
        if staged != request {
            return Err(CommitError::Request {
                staged,
                asked: request,
            });
        }
        let Some(p) = self.prepared.take() else {
            return Err(CommitError::NothingStaged);
        };
        let Some(screen) = &mut self.screen else {
            return Err(CommitError::Device);
        };
        if let Err(e) = screen.commit(p.token) {
            screen.discard();
            return Err(e);
        }
        if p.skin.is_some() {
            self.committed = p.skin;
        }
        Ok(())
    }

    /// Drops the preparation of `request`, in progress or answered, unless
    /// committed: what it staged never installs.
    fn discard(&mut self, request: u64) {
        if self.attempt.as_ref().is_some_and(|p| p.request == request) {
            self.attempt = None;
            if let Some(screen) = &mut self.screen {
                screen.discard();
            }
        }
        if self
            .prepared
            .as_ref()
            .is_some_and(|p| p.token.request == request)
        {
            self.drop_prepared();
        }
    }

    /// Drops the preparation answered ready and not committed, if any.
    fn drop_prepared(&mut self) {
        if self.prepared.take().is_some()
            && let Some(screen) = &mut self.screen
        {
            screen.discard();
        }
    }

    /// Prepares the chart loaded last and resets the screen's play state,
    /// building it again after a failure.
    fn prepare(&mut self) -> Result<(), RenderError> {
        let chart = self.chart.as_deref();
        let section = self.section.as_ref();
        let ready = match (&mut self.screen, chart) {
            (Some(s), Some(chart)) => s.load(chart, section),
            (Some(_), None) => Ok(()),
            (None, _) => build(&mut self.make, self.committed.as_ref(), chart, section).map(|s| {
                self.screen = Some(s);
                // The new screen takes the window's size afresh.
                self.applied = None;
            }),
        };
        if ready.is_ok() {
            self.shared.failed.store(false, Ordering::Release);
        } else {
            self.screen = None;
            self.granted = false;
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use bms_model::{Chart, ChartBuilder, Lane, TimeSource, TimeUs};
    use eval::{DeviceProfile, InputStart};
    use game::{Logic, LogicConfig};
    use input::{QpcClock, SourceReader};
    use render::{FrameInput, FrameTiming, RenderError, SkinError};

    use game::Preparation;

    use super::{MakeScreen, NoFrames, RenderMsg, Screen, ScreenError, View, spawn};
    use crate::attempt_context::AttemptContext;
    use crate::loading_text::{LoadingUpdate, Stage};
    use crate::measure::{Measure, PlayMeasure};

    fn timing(presented: bool) -> FrameTiming {
        FrameTiming {
            cpu: Duration::from_micros(50),
            sprites: 0,
            dropped: 0,
            presented,
            present_count: None,
        }
    }

    /// A measurement whose queues nothing feeds.
    fn measure(clock: QpcClock) -> Measure {
        let (_steps, steps) = rtrb::RingBuffer::new(1);
        let (_sounds, sounds) = rtrb::RingBuffer::new(1);
        let overlay = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Measure::new(clock, None, None, steps, sounds, overlay)
    }

    fn play_measure(origin: TimeUs) -> Box<PlayMeasure> {
        Box::new(PlayMeasure {
            generation: 1,
            origin,
            countdown_at: origin,
            started: origin,
            page_faults: None,
            load_from: TimeUs(0),
            retry: false,
            audio: None,
            queued_periods: None,
        })
    }

    /// A screen whose third text frame fails, and whose play frames are
    /// presented only from the fourth on, when it notes the clock.
    struct Flaky {
        text_frames: u32,
        play_frames: u32,
        clock: QpcClock,
        presented_at: Arc<AtomicI64>,
    }

    impl Screen for Flaky {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.play_frames += 1;
            let presented = self.play_frames >= 4;
            if presented {
                let _ = self.presented_at.compare_exchange(
                    i64::MIN,
                    self.clock.now().0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            Ok(timing(presented))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            self.text_frames += 1;
            if self.text_frames == 3 {
                Err(RenderError::SurfaceLost)
            } else {
                Ok(true)
            }
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A menu screen, the `id`th built, whose first screen fails its third
    /// menu frame; every menu frame notes the screen and whether its page
    /// was new.
    struct Menus {
        id: u32,
        frames: u32,
        seen: Arc<std::sync::Mutex<Vec<(u32, bool)>>>,
    }

    impl Screen for Menus {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.frames += 1;
            self.seen.lock().unwrap().push((self.id, changed));
            if self.id == 1 && self.frames == 3 {
                Err(RenderError::SurfaceLost)
            } else {
                Ok(timing(true))
            }
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The select screen is drawn without a chart, its page laid out once;
    /// a failed menu frame is reported, and after the preparation that
    /// builds the screen again, the page selection sends again is drawn
    /// there.
    #[test]
    fn a_failed_menu_frame_is_reported_and_the_page_draws_on_a_new_screen() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicU32::new(0));
        let (made, noted) = (Arc::clone(&builds), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            let id = made.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(Menus {
                id,
                frames: 0,
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        assert!(link.prepare().is_ok());
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| seen.lock().unwrap().contains(&(2, false))));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(&seen[..3], [(1, true), (1, false), (1, false)]);
        assert_eq!(seen[3], (2, true), "{seen:?}");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// The loading state is drawn on through a chart's load and a
    /// preparation, answered between its frames, until the play's page
    /// replaces it; after the select page, a load leaves nothing drawn
    /// until what comes next.
    #[test]
    fn the_loading_state_is_drawn_through_the_load_and_the_preparation() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Menus {
                // Not the first build: no frame fails.
                id: 2,
                frames: 0,
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let frames = || seen.lock().unwrap().len();
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.loading().show(crate::menu_screen::MenuPage::default());
        assert!(wait_until(|| frames() >= 2));
        link.load(Arc::clone(&chart), None).unwrap();
        let loaded = frames();
        assert!(
            wait_until(|| frames() >= loaded + 3),
            "no frame after the load"
        );
        link.prepare().unwrap();
        let prepared = frames();
        assert!(
            wait_until(|| frames() >= prepared + 3),
            "no frame after the preparation"
        );
        // The select page and a load: nothing drawn after the answer.
        link.send(RenderMsg::Menu(Box::default()));
        let menu = frames();
        assert!(wait_until(|| frames() >= menu + 2));
        link.load(chart, None).unwrap();
        let after = frames();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(frames(), after);
        link.exit();
    }

    /// A menu screen whose frames stall while `stall` is set; every menu
    /// frame notes its page's first line and whether the page was new.
    struct StalledMenus {
        stall: Arc<AtomicBool>,
        stalled: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
    }

    impl Screen for StalledMenus {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.seen.lock().unwrap().push((line, changed));
            while self.stall.load(Ordering::Acquire) {
                self.stalled.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.stalled.store(false, Ordering::Release);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    fn one_line(text: &str) -> crate::menu_screen::MenuPage {
        crate::menu_screen::MenuPage {
            lines: vec![crate::menu_screen::MenuLine {
                text: text.to_owned(),
                style: crate::menu_screen::LineStyle::Title,
            }],
            caret: None,
        }
    }

    /// A render thread stalled in a loading frame falls behind on nothing:
    /// of 300 pages shown meanwhile, only the newest is drawn, one new page
    /// once it resumes, and the play's preparation is answered at once
    /// after it; a page shown before loading ended is never drawn after.
    #[test]
    fn a_stalled_render_thread_draws_only_the_newest_loading_lines() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let sink = link.loading();
        let frames = || seen.lock().unwrap().len();
        sink.show(one_line("0"));
        assert!(wait_until(|| frames() >= 1));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        let before = frames();
        for n in 1..=300 {
            sink.show(one_line(&n.to_string()));
        }
        stall.store(false, Ordering::Release);
        let released = Instant::now();
        link.prepare().unwrap();
        // Unbacked by a slot, 300 pages a frame each would take seconds.
        assert!(released.elapsed() < Duration::from_secs(1));
        let after: Vec<_> = seen.lock().unwrap()[before..].to_vec();
        let new: Vec<_> = after.iter().filter(|(_, changed)| *changed).collect();
        assert_eq!(new, [&("300".to_owned(), true)], "{after:?}");

        // Loading ends while the thread stalls with a page waiting: the
        // select page follows, and the waiting page is never drawn.
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        sink.show(one_line("stale"));
        link.clear_loading();
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        stall.store(false, Ordering::Release);
        assert!(wait_until(|| seen
            .lock()
            .unwrap()
            .iter()
            .any(|(l, _)| l == "select")));
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.iter().any(|(l, _)| l == "stale"), "{seen:?}");
    }

    /// A paced screen, as a swap chain's waitable object paces one: `wait`
    /// grants a frame, blocking while `hold` is set, and the frame drawn
    /// next takes the grant; every frame notes whether it was a loading
    /// frame and its page's first line.
    struct PacedLoading {
        hold: Arc<AtomicBool>,
        waiting: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<(bool, String)>>>,
    }

    impl PacedLoading {
        fn note(&self, loading: bool, page: &crate::menu_screen::MenuPage) -> FrameTiming {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.seen.lock().unwrap().push((loading, line));
            timing(true)
        }
    }

    impl Screen for PacedLoading {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            while self.hold.load(Ordering::Acquire) {
                self.waiting.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.waiting.store(false, Ordering::Release);
            true
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(self.note(false, page))
        }
        fn loading(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            _update: Option<&crate::loading_text::LoadingUpdate>,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(self.note(true, page))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The end of loading before recording opens, while the render thread
    /// waits for its next frame: the answer comes only once it can draw
    /// no loading frame any more, and none is drawn after the end began,
    /// though the wait it was in grants a frame.
    #[test]
    fn no_loading_frame_follows_the_end_of_loading() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(AtomicBool::new(false));
        let waiting = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&hold), Arc::clone(&waiting));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(PacedLoading {
                hold: Arc::clone(&held),
                waiting: Arc::clone(&at),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let loading = || seen.lock().unwrap().iter().filter(|(l, _)| *l).count();
        link.loading().show(one_line("L"));
        assert!(wait_until(|| loading() >= 2));
        // The render thread waits for its next frame when loading ends.
        hold.store(true, Ordering::Release);
        assert!(wait_until(|| waiting.load(Ordering::Acquire)));
        let before = loading();
        let release = {
            let hold = Arc::clone(&hold);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                hold.store(false, Ordering::Release);
            })
        };
        link.end_loading().unwrap();
        let answered = loading();
        release.join().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        assert_eq!(answered, before, "a loading frame before the answer");
        assert_eq!(loading(), before, "a loading frame after the answer");
    }

    /// The end of loading is refused, so recording does not open, when the
    /// render thread stalls in a loading frame past the answer's bound, and
    /// when it is gone.
    #[test]
    fn an_unconfirmed_end_of_loading_is_an_error() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_secs(5));
        link.loading().show(one_line("L"));
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        assert!(matches!(link.end_loading(), Err(ScreenError::NoAnswer)));
        stall.store(false, Ordering::Release);
        // The thread ends; nothing answers any more.
        link.send(RenderMsg::Exit);
        assert!(wait_until(
            || link.ended.try_recv() == Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert!(matches!(link.end_loading(), Err(ScreenError::Ended)));
    }

    /// A load cancelled while the render thread stalls with its signal
    /// unread, the select page sent and another chart chosen at once: the
    /// cancelled load's signal takes nothing, and the next load's lines
    /// get a signal of their own, drawn after the select page and kept
    /// through its chart's load.
    #[test]
    fn a_cancelled_loads_signal_leaves_the_next_loads_lines() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        // Chart A: its lines and signal, then the quit key cancels it and
        // selection is entered; chart B is chosen at once.
        link.loading().show(one_line("A"));
        link.clear_loading();
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        link.loading().show(one_line("B"));
        stall.store(false, Ordering::Release);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(chart, None).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        let lines: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
        assert!(!lines.contains(&"A"), "{lines:?}");
        let last_select = lines.iter().rposition(|l| *l == "select").unwrap();
        assert!(
            lines[last_select..].contains(&"B"),
            "B not drawn after the select page: {lines:?}"
        );
        assert_eq!(lines.last(), Some(&"B"), "{lines:?}");
    }

    /// A screen that cannot draw any chart; it notes its menu frames by the
    /// number of the build that made it.
    struct NoCharts {
        id: u32,
        seen: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl Screen for NoCharts {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Err(RenderError::Skin(SkinError {
                reason: "no lanes".to_owned(),
            }))
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.seen.lock().unwrap().push(self.id);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A screen with a splash page: it counts its splash and menu frames.
    struct Splashes {
        splashes: Arc<AtomicU32>,
        menus: Arc<AtomicU32>,
    }

    impl Screen for Splashes {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn splash(&mut self, _elapsed: Duration) -> Option<Result<bool, RenderError>> {
            self.splashes.fetch_add(1, Ordering::SeqCst);
            Some(Ok(true))
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.menus.fetch_add(1, Ordering::SeqCst);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread draws the splash page from its start, before the
    /// game sends anything, and only until the first page replaces it.
    #[test]
    fn the_splash_shows_out_its_time_then_the_first_page() {
        let (splashes, menus) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (s, m) = (Arc::clone(&splashes), Arc::clone(&menus));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Splashes {
                splashes: Arc::clone(&s),
                menus: Arc::clone(&m),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let spawned = Instant::now();
        assert!(wait_until(|| splashes.load(Ordering::SeqCst) >= 2));
        // The page, ready early, waits for the launch screen's end.
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= 1));
        assert!(spawned.elapsed() >= crate::menu_screen::SPLASH);
        let drawn = splashes.load(Ordering::SeqCst);
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= 3));
        link.exit();
        assert_eq!(splashes.load(Ordering::SeqCst), drawn);
    }

    /// A page that comes after the launch screen's time replaces it at
    /// once.
    #[test]
    fn a_page_after_the_splash_time_shows_at_once() {
        let (splashes, menus) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (s, m) = (Arc::clone(&splashes), Arc::clone(&menus));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Splashes {
                splashes: Arc::clone(&s),
                menus: Arc::clone(&m),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        std::thread::sleep(crate::menu_screen::SPLASH + Duration::from_millis(100));
        assert_eq!(menus.load(Ordering::SeqCst), 0);
        let sent = Instant::now();
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= 1));
        assert!(
            sent.elapsed() < Duration::from_millis(500),
            "{:?}",
            sent.elapsed()
        );
        link.exit();
    }

    /// A preparation (a play's, before it opens) leaves the splash drawn,
    /// whether it comes before the splash's first frame or after: the
    /// hidden window shows on a presented frame, and the play waits for
    /// the show.
    #[test]
    fn a_preparation_keeps_the_splash_drawn() {
        for prepare_first in [true, false] {
            let splashes = Arc::new(AtomicU32::new(0));
            let s = Arc::clone(&splashes);
            let make: MakeScreen = Box::new(move || {
                Ok(Box::new(Splashes {
                    splashes: Arc::clone(&s),
                    menus: Arc::new(AtomicU32::new(0)),
                }) as Box<dyn Screen>)
            });
            let clock = QpcClock::start();
            let link = spawn(
                make,
                None,
                None,
                clock,
                view(),
                measure(clock),
                Box::new(NoFrames),
            )
            .unwrap();
            if !prepare_first {
                assert!(wait_until(|| splashes.load(Ordering::SeqCst) >= 1));
            }
            assert!(link.prepare().is_ok());
            let drawn = splashes.load(Ordering::SeqCst);
            assert!(
                wait_until(|| splashes.load(Ordering::SeqCst) >= drawn + 2),
                "prepare first: {prepare_first}"
            );
            link.exit();
        }
    }

    /// A screen that notes the section of every chart it prepares.
    struct Sections(Arc<std::sync::Mutex<Vec<Option<eval::Section>>>>);

    impl Screen for Sections {
        fn load(
            &mut self,
            _chart: &Chart,
            section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            self.0.lock().unwrap().push(section.cloned());
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The section a chart is spawned or loaded with reaches the screen;
    /// the screen's recovery prepares nothing, and every attempt prepares
    /// the chart with its own section, or none for the whole chart.
    #[test]
    fn each_attempt_prepares_with_its_own_section() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Sections(Arc::clone(&noted))) as Box<dyn Screen>));
        let section = |s: i64| eval::Section {
            start: TimeUs(s),
            end: TimeUs(s + 1_000_000),
            lead_in: TimeUs(2_000_000),
            initial_gauges: Vec::new(),
        };
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::clone(&chart)),
            Some(section(1)),
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.load(Arc::clone(&chart), Some(section(2))).unwrap();
        link.prepare().unwrap();
        let ready = link.prepare_attempt(attempt(&chart, Some(&section(3))), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.prepare().unwrap();
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        assert_eq!(
            *seen.lock().unwrap(),
            [Some(section(1)), Some(section(2)), Some(section(3)), None]
        );
        link.exit();
    }

    /// The attempt of `chart` the game would make: the default preset,
    /// the whole chart or `section`'s round.
    fn attempt(chart: &Chart, section: Option<&eval::Section>) -> AttemptContext {
        let attempt = game::Attempt::new(
            1,
            &rules::tapestry(),
            TimeUs(1_000_000),
            section,
            game::NoteEnd::Play.of(chart),
            &game::ChartLabels::default(),
        );
        AttemptContext::new(attempt, crate::attempt_context::SIMPLE_SCRATCH)
    }

    /// A render thread on the screens `make` builds, with no chart, no
    /// section and no frame sink.
    fn spawn_empty(make: MakeScreen) -> super::RenderLink {
        let clock = QpcClock::start();
        spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap()
    }

    /// A screen whose skin refuses every section's attempt and fails its
    /// device on a whole chart's while `lost` is set; it notes its menu
    /// frames by the number of the build that made it.
    struct Refusing {
        id: u32,
        lost: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl Screen for Refusing {
        fn load(
            &mut self,
            _chart: &Chart,
            section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            if section.is_some() {
                Err(RenderError::Skin(SkinError {
                    reason: "no section".to_owned(),
                }))
            } else if self.lost.load(Ordering::Acquire) {
                Err(RenderError::SurfaceLost)
            } else {
                Ok(())
            }
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.seen.lock().unwrap().push(self.id);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// An attempt the skin refuses is answered with the skin's reason,
    /// reports no failure and keeps the screen, which goes on drawing the
    /// menus without being built again; a device failure while preparing
    /// is an error that drops the screen, which the recovery builds again.
    #[test]
    fn a_refused_attempt_keeps_the_screen_and_reports_no_failure() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let lost = Arc::new(AtomicBool::new(false));
        let builds = Arc::new(AtomicU32::new(0));
        let (made, noted, gone) = (Arc::clone(&builds), Arc::clone(&seen), Arc::clone(&lost));
        let make: MakeScreen = Box::new(move || {
            let id = made.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(Refusing {
                id,
                lost: Arc::clone(&gone),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        let round = eval::Section {
            start: TimeUs(0),
            end: TimeUs(1_000_000),
            lead_in: TimeUs(2_000_000),
            initial_gauges: Vec::new(),
        };
        let refused = link.prepare_attempt(attempt(&chart, Some(&round)), || false);
        assert_eq!(
            refused.unwrap(),
            Preparation::Refused(game::PreparationRefusal {
                location: None,
                reason: "no section".to_owned(),
            })
        );
        assert!(!link.failed(), "a refusal reported a failure");
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| seen.lock().unwrap().len() >= 3));
        assert!(!link.failed());
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "the screen was built again"
        );
        assert!(seen.lock().unwrap().iter().all(|id| *id == 1));
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);

        lost.store(true, Ordering::Release);
        let failed = link.prepare_attempt(attempt(&chart, None), || false);
        assert!(
            matches!(failed, Err(ScreenError::Render(RenderError::SurfaceLost))),
            "{failed:?}"
        );
        lost.store(false, Ordering::Release);
        link.prepare().unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        link.exit();
    }

    /// A screen whose chart preparation blocks while `hold` is set.
    struct Held(Arc<AtomicBool>);

    impl Screen for Held {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            while self.0.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A slow attempt's preparation stops as soon as the player leaves or
    /// the input or audio thread fails, without waiting for the screen's
    /// answer; the game then looks at the threads for the cause.
    #[test]
    fn a_slow_attempt_stops_when_the_player_leaves_or_a_thread_fails() {
        let hold = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&hold);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Held(Arc::clone(&held))) as Box<dyn Screen>));
        let link = spawn_empty(make);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        for cause in ["leave", "input", "audio"] {
            hold.store(true, Ordering::Release);
            let started = Instant::now();
            let at = started + Duration::from_millis(100);
            let answer = link.prepare_attempt(attempt(&chart, None), || {
                let now = Instant::now() >= at;
                crate::play::preparation_stops(
                    false,
                    now && cause == "leave",
                    (now && cause == "input").then_some(game::InputWaitError::Gone),
                    (now && cause == "audio").then_some(audio::OutputError::DeviceLost),
                )
            });
            assert_eq!(answer.unwrap(), Preparation::Cancelled, "{cause}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{cause}: {:?}",
                started.elapsed()
            );
            hold.store(false, Ordering::Release);
        }
        link.exit();
    }

    /// A loading frame that fails after the attempt was prepared drops the
    /// screen: the end of loading says so, and recording must not open.
    #[test]
    fn the_end_of_loading_reports_a_screen_lost_after_the_preparation() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Menus {
                // The first build: its third frame fails.
                id: 1,
                frames: 0,
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.loading().show(one_line("L"));
        assert!(wait_until(|| link.failed()));
        assert!(matches!(link.end_loading(), Err(ScreenError::Lost)));
        link.exit();
    }

    /// A screen that notes every loading frame's first line and typed
    /// update.
    type Typed = Arc<std::sync::Mutex<Vec<(String, Option<LoadingUpdate>)>>>;

    struct TypedLoading {
        seen: Typed,
    }

    impl Screen for TypedLoading {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn loading(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            update: Option<&LoadingUpdate>,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.seen.lock().unwrap().push((line, update.cloned()));
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The typed update shown with a loading page reaches the screen with
    /// that page; a page shown without one carries none, not the one
    /// before; and an update of an ended loading lifetime is never drawn.
    #[test]
    fn typed_loading_updates_travel_with_their_page_and_lifetime() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(TypedLoading {
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let update = |stage, title: &str| LoadingUpdate {
            stage,
            chart: crate::loading_text::ChosenChart {
                title: Some(title.to_owned()),
                genre: Some("Genre".to_owned()),
                ..crate::loading_text::ChosenChart::default()
            },
            preset: "preset".to_owned(),
            practice: false,
        };
        let drawn = |line: &str| {
            let line = line.to_owned();
            let seen = Arc::clone(&seen);
            move || seen.lock().unwrap().iter().any(|(l, _)| *l == line)
        };
        let sink = link.loading();
        let chart_stage = update(Stage::Chart, "One");
        sink.show_typed(one_line("A"), chart_stage.clone());
        assert!(wait_until(drawn("A")));
        sink.show(one_line("B"));
        assert!(wait_until(drawn("B")));
        // The old lifetime's sink after loading ended, then the next one's.
        link.clear_loading();
        sink.show_typed(one_line("stale"), update(Stage::Loudness, "Old"));
        let next = link.loading();
        let sounds = update(Stage::Sounds { done: 1, files: 2 }, "Two");
        next.show_typed(one_line("C"), sounds.clone());
        assert!(wait_until(drawn("C")));
        std::thread::sleep(Duration::from_millis(50));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        let first = |line: &str| seen.iter().find(|(l, _)| l == line).map(|(_, u)| u.clone());
        assert_eq!(first("A"), Some(Some(chart_stage)), "{seen:?}");
        assert_eq!(first("B"), Some(None), "{seen:?}");
        assert_eq!(first("C"), Some(Some(sounds)), "{seen:?}");
        assert!(!seen.iter().any(|(l, _)| l == "stale"), "{seen:?}");
    }

    /// A chart the screen cannot draw is refused, and the select screen
    /// still draws afterwards, on a screen built again without the chart:
    /// no failure is left for the game to find.
    #[test]
    fn a_refused_chart_leaves_a_working_select_screen() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicU32::new(0));
        let (made, noted) = (Arc::clone(&builds), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            let id = made.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(NoCharts {
                id,
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        assert!(matches!(
            link.load(chart, None),
            Err(ScreenError::Render(RenderError::Skin(_)))
        ));
        assert!(!link.failed());
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| seen.lock().unwrap().contains(&2)));
        assert!(!link.failed());
        link.exit();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// A screen whose chart preparation blocks until `release` sends.
    struct Blocking {
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Screen for Blocking {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            let _ = self.release.recv();
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A play's figures are waited for as long as the bound set says: a
    /// render thread blocked in a chart's load does not answer the play's
    /// end, and the wait gives up at the bound, far below the production
    /// one.
    #[test]
    fn a_blocked_thread_bounds_the_figures_wait() {
        let (release, blocked) = std::sync::mpsc::channel();
        let mut blocked = Some(blocked);
        let make: MakeScreen = Box::new(move || {
            let release = blocked.take().expect("built once");
            Ok(Box::new(Blocking { release }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        assert!(matches!(link.load(chart, None), Err(ScreenError::NoAnswer)));
        let started = Instant::now();
        assert!(link.finish(clock.now()).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(link.exit(), super::RenderEnd::Stuck);
        release.send(()).unwrap();
    }

    /// A chart's load that blocks the render thread bounds every wait on
    /// it: the load and a preparation give up, and the close that follows
    /// returns, saying the thread did not end, so the window stays open.
    #[test]
    fn a_blocked_load_bounds_the_preparation_and_the_close() {
        let (release, blocked) = std::sync::mpsc::channel();
        let mut blocked = Some(blocked);
        let make: MakeScreen = Box::new(move || {
            let release = blocked.take().expect("built once");
            Ok(Box::new(Blocking { release }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let started = Instant::now();
        assert!(matches!(link.load(chart, None), Err(ScreenError::NoAnswer)));
        assert!(matches!(link.prepare(), Err(ScreenError::NoAnswer)));
        assert_eq!(link.exit(), super::RenderEnd::Stuck);
        assert!(started.elapsed() < Duration::from_secs(5));
        // The thread ends once its screen does.
        release.send(()).unwrap();
    }

    fn wait_until(f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn view() -> View {
        View {
            tuning: Arc::new(crate::tuning::LiveTuning::new(game::Tuning {
                visible_time_ms: 600,
                volume_percent: 100,
                display_offset_us: 0,
                input_offset_us: 0,
            })),
            effects: 16,
        }
    }

    /// Builds up to `ok` screens, then fails; counts the builds.
    fn factory(
        ok: u32,
        clock: QpcClock,
        presented_at: &Arc<AtomicI64>,
    ) -> (Arc<AtomicU32>, MakeScreen) {
        let builds = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&builds);
        let presented_at = Arc::clone(presented_at);
        let make = Box::new(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= ok {
                Ok(Box::new(Flaky {
                    text_frames: 0,
                    play_frames: 0,
                    clock,
                    presented_at: Arc::clone(&presented_at),
                }) as Box<dyn Screen>)
            } else {
                Err(RenderError::Skin(SkinError {
                    reason: format!("build {n} failed"),
                }))
            }
        });
        (builds, make)
    }

    /// A render error passes through transparently: its text and its
    /// source are the renderer's. The thread's own failures keep theirs.
    #[test]
    fn screen_errors_keep_their_sources() {
        use std::error::Error as _;

        let skin = SkinError {
            reason: "no lanes".to_owned(),
        };
        let e = ScreenError::from(RenderError::Skin(skin.clone()));
        assert_eq!(e.to_string(), "skin");
        let source = e.source().expect("the skin error is the source");
        assert_eq!(source.downcast_ref::<SkinError>(), Some(&skin));
        assert!(source.source().is_none());
        assert_eq!(game::Chain(&e).to_string(), "skin: no lanes");

        let e = ScreenError::Spawn(std::io::Error::other("no threads"));
        assert_eq!(e.to_string(), "the render thread cannot start");
        let source = e.source().expect("the spawn error is the source");
        assert!(source.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(
            game::Chain(&e).to_string(),
            "the render thread cannot start: no threads"
        );
        assert!(ScreenError::Ended.source().is_none());
        assert!(ScreenError::EndedWhileStarting.source().is_none());
    }

    #[test]
    fn a_failed_frame_is_reported_and_only_a_preparation_builds_again() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let clock = QpcClock::start();
        let presented = Arc::new(AtomicI64::new(i64::MIN));
        let (builds, make) = factory(2, clock, &presented);
        let link = spawn(
            make,
            Some(chart),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        assert!(!link.failed());
        link.show_end(super::EndPage {
            lines: vec!["A".to_owned()],
            graphs: None,
        });
        assert!(wait_until(|| link.failed()), "the failure was not reported");
        // More text without a screen builds nothing and stays failed.
        link.show_end(super::EndPage {
            lines: vec!["A".to_owned()],
            graphs: None,
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(link.failed());
        // The next preparation builds the screen again.
        assert!(link.prepare().is_ok());
        assert!(!link.failed());
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        link.show_end(super::EndPage {
            lines: vec!["B".to_owned()],
            graphs: None,
        });
        assert!(wait_until(|| link.failed()));
        // Building again fails: the preparation reports it.
        let refused = link.prepare();
        assert!(
            matches!(
                &refused,
                Err(ScreenError::Render(RenderError::Skin(SkinError { reason })))
                    if reason == "build 3 failed"
            ),
            "{refused:?}"
        );
        link.exit();
    }

    /// What [`RefusingFrames`] saw, in counters that never allocate: its
    /// preparations, its build calls, the preparation the last build came
    /// in, its draws and its end screen frames.
    #[derive(Default)]
    struct FrameCalls {
        epoch: AtomicU32,
        builds: AtomicU32,
        last_build_epoch: AtomicU32,
        draws: AtomicU32,
        texts: AtomicU32,
    }

    /// A screen whose skin refuses every frame while `refuse` is set,
    /// counting what it is called for. It refuses as told, without the
    /// renderer's own rule of refusing every later frame of a refused
    /// preparation, so only the render thread keeps it from being built
    /// again.
    struct RefusingFrames {
        calls: Arc<FrameCalls>,
        refuse: Arc<AtomicBool>,
    }

    impl Screen for RefusingFrames {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            self.calls.epoch.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            let epoch = self.calls.epoch.load(Ordering::Acquire);
            self.calls.last_build_epoch.store(epoch, Ordering::Release);
            self.calls.builds.fetch_add(1, Ordering::AcqRel);
            if self.refuse.load(Ordering::Acquire) {
                return Err(skin_model::BuildError::InconsistentTally);
            }
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.calls.draws.fetch_add(1, Ordering::AcqRel);
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            self.calls.texts.fetch_add(1, Ordering::AcqRel);
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A refused frame stops the attempt without dropping the screen: it
    /// is reported as the screen's failure, so the game ends the play, and
    /// no frame of that preparation is built or drawn again, however many
    /// frame opportunities follow. The recovery keeps the screen, built
    /// once, and only the next attempt's preparation builds again. The
    /// app region ends on the refusal's way out: the end screen drawn after
    /// it, which allocates, counts nothing in it (dev-tools).
    #[test]
    fn a_refused_frame_stops_the_attempt_and_keeps_the_screen() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(60_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let play = || {
            Logic::<SourceReader>::new(
                &chart,
                &rule,
                &device,
                &InputStart::default(),
                Vec::new(),
                LogicConfig {
                    inputs: 0,
                    effects: 16,
                },
                Vec::new(),
            )
            .unwrap()
        };
        let calls = Arc::new(FrameCalls::default());
        let refuse = Arc::new(AtomicBool::new(false));
        let made = Arc::new(AtomicU32::new(0));
        let (seen, refusing, count) = (Arc::clone(&calls), Arc::clone(&refuse), Arc::clone(&made));
        let make: MakeScreen = Box::new(move || {
            count.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(RefusingFrames {
                calls: Arc::clone(&seen),
                refuse: Arc::clone(&refusing),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let shared_chart = Arc::new(chart.clone());
        let link = spawn(
            make,
            Some(Arc::clone(&shared_chart)),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let ready = link.prepare_attempt(attempt(&shared_chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        let first = calls.epoch.load(Ordering::Acquire);
        let (_logic, side) = play();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        assert!(wait_until(|| calls.draws.load(Ordering::Acquire) >= 3));
        assert!(!link.failed());

        refuse.store(true, Ordering::Release);
        assert!(wait_until(|| link.failed()), "the refusal was not reported");
        let (builds, draws) = (
            calls.builds.load(Ordering::Acquire),
            calls.draws.load(Ordering::Acquire),
        );
        assert_eq!(calls.last_build_epoch.load(Ordering::Acquire), first);
        // Frame opportunities: each wake goes round the render loop.
        for _ in 0..5 {
            link.send(RenderMsg::Resize);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(calls.builds.load(Ordering::Acquire), builds, "built again");
        assert_eq!(calls.draws.load(Ordering::Acquire), draws, "drawn again");
        link.show_end(super::EndPage {
            lines: vec!["END".to_owned()],
            graphs: None,
        });
        assert!(wait_until(|| calls.texts.load(Ordering::Acquire) >= 1));
        let fig = link.finish(clock.now()).expect("the play's figures");
        #[cfg(feature = "dev-tools")]
        assert_eq!(fig.render_app_allocations, Some(0));
        #[cfg(not(feature = "dev-tools"))]
        assert_eq!(fig.render_app_allocations, None);
        assert_eq!(calls.builds.load(Ordering::Acquire), builds);

        // The recovery keeps the screen; the next attempt's preparation
        // builds again, in its own preparation.
        link.prepare().unwrap();
        assert!(!link.failed());
        refuse.store(false, Ordering::Release);
        let ready = link.prepare_attempt(attempt(&shared_chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        let next = calls.epoch.load(Ordering::Acquire);
        assert!(next > first);
        let (_next_logic, side) = play();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        assert!(wait_until(
            || calls.draws.load(Ordering::Acquire) >= draws + 3
        ));
        assert!(calls.builds.load(Ordering::Acquire) > builds);
        assert_eq!(calls.last_build_epoch.load(Ordering::Acquire), next);
        assert!(!link.failed());
        assert_eq!(
            made.load(Ordering::Acquire),
            1,
            "the screen was built again"
        );
        let _ = link.finish(clock.now());
        link.exit();
    }

    #[test]
    fn the_first_frame_is_the_first_presented_one() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let rule = rules::tapestry();
        let device = DeviceProfile::default();
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let clock = QpcClock::start();
        let presented = Arc::new(AtomicI64::new(i64::MIN));
        let (_builds, make) = factory(1, clock, &presented);
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        assert!(wait_until(|| link.first_frame().is_some()));
        let first = link.first_frame().unwrap().0;
        let at = presented.load(Ordering::Acquire);
        assert_ne!(at, i64::MIN);
        assert!(first >= at, "first frame at {first}, presented at {at}");
        link.exit();
    }

    /// A screen that keeps each play frame's visible time and display
    /// offset.
    struct Inputs(Arc<std::sync::Mutex<Vec<(TimeUs, TimeUs)>>>);

    impl Screen for Inputs {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            self.0
                .lock()
                .unwrap()
                .push((f.visible_time, f.display_offset));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread reads hi-speed and the display offset at every
    /// frame: each change reaches the frames drawn after it, and no later
    /// frame goes back. The two change one at a time, as two keys do.
    #[test]
    fn render_frames_follow_hi_speed_and_display_offset() {
        use game::Adjust;

        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let tuning = view().tuning;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let screen_seen = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Inputs(Arc::clone(&screen_seen))) as Box<dyn Screen>));
        let clock = QpcClock::start();
        let view = View {
            tuning: Arc::clone(&tuning),
            effects: 16,
        };
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view,
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        let start = (TimeUs(600_000), TimeUs(0));
        let faster = (TimeUs(569_000), TimeUs(0));
        let later = (TimeUs(569_000), TimeUs(1_000));
        assert!(wait_until(|| seen.lock().unwrap().contains(&start)));
        assert!(tuning.adjust(Adjust::HiSpeedUp));
        assert!(wait_until(|| seen.lock().unwrap().contains(&faster)));
        assert!(tuning.adjust(Adjust::DisplayOffsetUp));
        assert!(wait_until(|| seen.lock().unwrap().contains(&later)));
        link.exit();
        // The frames go through the three states in order, each once.
        let mut frames = seen.lock().unwrap().clone();
        frames.dedup();
        assert_eq!(frames, [start, faster, later]);
    }

    /// A screen whose every play frame fails, as a lost surface does.
    struct Lost;

    impl Screen for Lost {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Err(RenderError::SurfaceLost)
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A judgment drawn on a play frame that fails never reaches the screen
    /// through it: the render thread counts it N/A in the play's figures.
    #[test]
    fn a_failed_play_frame_makes_its_judgments_na() {
        use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId};
        use game::DeviceSettings;
        use input::DeviceKind;

        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (mut writer, reader) = input::source_queue(16);
        // Z, lane 1's default key, pressed on the note.
        let press = PhysicalInputEvent {
            received: TimeUs(1_000_000),
            source: SourceId(0),
            kind: InputKind::Control {
                control: ControlId(0x2c),
                value: 1,
            },
        };
        writer.push(press).unwrap();
        writer.publish(TimeUs(1_000_001));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 16,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        logic.advance_to(TimeUs(1_000_002));
        assert!(
            logic
                .events()
                .iter()
                .any(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
        );
        let clock = QpcClock::start();
        let make: MakeScreen = Box::new(|| Ok(Box::new(Lost) as Box<dyn Screen>));
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        assert!(wait_until(|| link.failed()));
        let fig = link.finish(clock.now()).expect("the play's figures");
        assert_eq!(fig.press_to_screen_unavailable, 1);
        assert_eq!(fig.press_to_screen, None);
        link.exit();
    }

    /// Play frames the [`Stalling`] screen draws before it stalls.
    const DRAWN_FRAMES: u32 = 2;

    /// A paced screen that lets [`DRAWN_FRAMES`] play frames through and
    /// then times out every wait; it keeps each frame's effect count and
    /// how many of them were judgments.
    struct Stalling {
        waits: Arc<AtomicU32>,
        seen: Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
    }

    impl Screen for Stalling {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            if self.waits.fetch_add(1, Ordering::AcqRel) < DRAWN_FRAMES {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
            false
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            let judged = f
                .effects
                .iter()
                .filter(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
                .count();
            self.seen.lock().unwrap().push((f.effects.len(), judged));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// More effects are queued than a frame's buffer holds: each frame
    /// draws at most a buffer's worth, and the rest wait in the queue.
    /// Judgments still queued when the play ends never reached the screen
    /// and are N/A in its figures, as are the drawn ones offscreen (no
    /// present count to pair them with).
    #[test]
    fn frames_draw_a_buffer_of_effects_and_the_rest_end_na() {
        use game::DeviceSettings;
        use input::DeviceKind;

        // The frame buffer the render thread makes for a view of 2 effects.
        let buffer = Vec::<eval::JudgeEvent>::with_capacity(2).capacity();
        let mut builder = ChartBuilder::new(120_000);
        for k in 0..6 {
            builder = builder.tap(Lane::new(1).unwrap(), TimeUs(1_000_000 + k * 200_000));
        }
        let chart = builder.build().unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (mut writer, reader) = input::source_queue(32);
        // Every note hit on time.
        for e in input::autoplay(&chart, &device.sources[0]).unwrap() {
            writer.push(e).unwrap();
        }
        writer.publish(TimeUs(3_000_000));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 32,
                effects: 64,
            },
            Vec::new(),
        )
        .unwrap();
        // Every effect is queued before the render thread draws a frame.
        logic.advance_to(TimeUs(3_000_001));
        let judged = logic
            .events()
            .iter()
            .filter(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
            .count();
        assert_eq!(judged, 6);
        // More than the drawn frames' buffers hold.
        assert!(logic.events().len() > DRAWN_FRAMES as usize * buffer);
        let waits = Arc::new(AtomicU32::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let screen = (Arc::clone(&waits), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Stalling {
                waits: Arc::clone(&screen.0),
                seen: Arc::clone(&screen.1),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let view = View {
            effects: buffer,
            ..view()
        };
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view,
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        // Past its last frame: the screen has stalled.
        assert!(wait_until(|| waits.load(Ordering::Acquire) > DRAWN_FRAMES));
        let fig = link.finish(clock.now()).expect("the play's figures");
        link.exit();
        let frames = seen.lock().unwrap().clone();
        assert_eq!(frames.len(), DRAWN_FRAMES as usize);
        // Each frame drew a full buffer, never more.
        assert!(frames.iter().all(|&(n, _)| n == buffer), "{frames:?}");
        let drawn: usize = frames.iter().map(|&(_, j)| j).sum();
        assert!(drawn < judged, "{frames:?}");
        // The drawn judgments and the ones still queued at the end.
        assert_eq!(fig.press_to_screen_unavailable, judged as u64);
        assert_eq!(fig.press_to_screen, None);
    }

    /// Something that allocates, kept by `black_box` so no profile elides it.
    #[cfg(feature = "dev-tools")]
    fn allocate() {
        drop(std::hint::black_box(vec![0u8; 8]));
    }

    /// A screen that allocates in every draw and read-back, and in every
    /// build when `in_build`, as wgpu, the readback and a faulty skin would;
    /// it counts its draws and read-backs.
    #[cfg(feature = "dev-tools")]
    struct Allocating {
        in_build: bool,
        draws: Arc<AtomicU32>,
        read_backs: Arc<AtomicU32>,
    }

    #[cfg(feature = "dev-tools")]
    impl Screen for Allocating {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            if self.in_build {
                allocate();
            }
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            allocate();
            self.draws.fetch_add(1, Ordering::AcqRel);
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            self.read_backs.fetch_add(1, Ordering::AcqRel);
            Some((1, 1, vec![0; 4]))
        }
    }

    /// A frame sink that reads every frame back and allocates, as the frame
    /// dump does; it counts its frames.
    #[cfg(feature = "dev-tools")]
    struct Dumping(Arc<AtomicU32>);

    #[cfg(feature = "dev-tools")]
    impl super::FrameSink for Dumping {
        fn frame(&mut self, screen: &dyn Screen, _drawn: &crate::measure::Drawn<'_>) {
            drop(std::hint::black_box(screen.read_back()));
            allocate();
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// A play drawn on [`Allocating`] into [`Dumping`], whose build
    /// allocates when `in_build` and whose measurement when `in_measure`:
    /// its app region count, and how many draws, read-backs and sunk frames
    /// ran.
    #[cfg(feature = "dev-tools")]
    fn app_region_count(in_build: bool, in_measure: bool) -> (Option<u64>, [u32; 3]) {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let counters = [(); 3].map(|()| Arc::new(AtomicU32::new(0)));
        let [draws, read_backs, sunk] = counters.clone();
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Allocating {
                in_build,
                draws: Arc::clone(&draws),
                read_backs: Arc::clone(&read_backs),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut m = measure(clock);
        if in_measure {
            m.allocate_in_frames();
        }
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            m,
            Box::new(Dumping(sunk)),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        let [_, _, sunk] = &counters;
        assert!(wait_until(|| sunk.load(Ordering::Acquire) >= 5));
        let fig = link.finish(clock.now()).expect("the play's figures");
        link.exit();
        (
            fig.render_app_allocations,
            counters.map(|c| c.load(Ordering::Acquire)),
        )
    }

    /// What the screen draws, reads back and the frame sink does allocates
    /// outside the app region: the region counts none of it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_leaves_out_drawing_and_the_frame_sink() {
        let (count, [draws, read_backs, sunk]) = app_region_count(false, false);
        assert_eq!(count, Some(0));
        assert!(draws >= 5 && read_backs >= 5 && sunk >= 5);
    }

    /// An allocation in the frame's build, the region's first interval, is
    /// counted.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_counts_the_frame_build() {
        let (count, _) = app_region_count(true, false);
        assert!(count.is_some_and(|n| n > 0), "{count:?}");
    }

    /// An allocation in a play frame's measurement, the region's second
    /// interval, is counted; the build allocates nothing here.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_counts_the_frame_measurement() {
        let (count, _) = app_region_count(false, true);
        assert!(count.is_some_and(|n| n > 0), "{count:?}");
    }

    /// A play's end is a barrier for a scenario's captures: the frame drawn
    /// before the end, which reaches the capture's time, has been written
    /// when the end is answered; no frame after it counts in the play, so
    /// a capture bound to it then is never taken.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_plays_end_settles_its_captures_after_its_last_frame() {
        use crate::dev::scenario::{CaptureState, Captures, ScenarioFrames};
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (taken, late) = (dir.path().join("taken.png"), dir.path().join("late.png"));
        let captures = Captures::of(vec![
            (TimeUs(2_900_000), taken.clone()),
            (TimeUs(0), late.clone()),
        ]);
        let draws = Arc::new(AtomicU32::new(0));
        let drawn = Arc::clone(&draws);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Allocating {
                in_build: false,
                draws: Arc::clone(&drawn),
                read_backs: Arc::new(AtomicU32::new(0)),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(ScenarioFrames::new(captures.clone())),
        )
        .unwrap();
        captures.arm(0);
        captures.bind(1);
        // Chart time 3 s at the play's first frame: past the capture's.
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(TimeUs(clock.now().0 - 3_000_000)),
        });
        assert!(link.finish(clock.now()).is_some());
        assert_eq!(captures.states()[0], CaptureState::Succeeded);
        assert!(std::fs::read(&taken).unwrap().starts_with(b"\x89PNG"));
        // Bound once the play's figures are taken: the frames still drawn
        // count in no play.
        captures.arm(1);
        captures.bind(1);
        // A frame drawn after the next has started was sunk whole.
        let after = draws.load(Ordering::Acquire);
        assert!(wait_until(|| draws.load(Ordering::Acquire) >= after + 2));
        assert_eq!(captures.states()[1], CaptureState::Bound(1));
        assert!(!late.exists());
        link.exit();
    }

    /// What an end frame drew: its first line and its graphs.
    type Drew = (String, Option<Arc<game::ResultGraphs>>);

    /// A screen that notes every end page it draws; while `hold` is set,
    /// an end frame waits for `release` once.
    struct Ends {
        drew: Arc<std::sync::Mutex<Vec<Drew>>>,
        hold: Arc<std::sync::atomic::AtomicBool>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Screen for Ends {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            panic!("an end page is drawn with its graphs")
        }
        fn end(&mut self, page: &super::EndPage) -> Result<bool, RenderError> {
            let first = page.lines.first().cloned().unwrap_or_default();
            self.drew.lock().unwrap().push((first, page.graphs.clone()));
            if self.hold.swap(false, Ordering::SeqCst) {
                let _ = self.release.recv_timeout(Duration::from_secs(10));
            }
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.drew.lock().unwrap().push(("<menu>".to_owned(), None));
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread over an [`Ends`] screen: the link, what it drew,
    /// the hold flag and the release.
    type Ended = (
        super::RenderLink,
        Arc<std::sync::Mutex<Vec<Drew>>>,
        Arc<std::sync::atomic::AtomicBool>,
        std::sync::mpsc::Sender<()>,
    );

    /// [`Ended`], started.
    fn ends() -> Ended {
        let drew = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, released) = std::sync::mpsc::channel();
        let mut released = Some(released);
        let (noted, held) = (Arc::clone(&drew), Arc::clone(&hold));
        let make: MakeScreen = Box::new(move || {
            let release = released.take().expect("built once");
            Ok(Box::new(Ends {
                drew: Arc::clone(&noted),
                hold: Arc::clone(&held),
                release,
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        (link, drew, hold, release)
    }

    fn page(line: &str, graphs: Option<&Arc<game::ResultGraphs>>) -> super::EndPage {
        super::EndPage {
            lines: vec![line.to_owned()],
            graphs: graphs.cloned(),
        }
    }

    fn graphs(reached: u16) -> Arc<game::ResultGraphs> {
        Arc::new(game::ResultGraphs {
            reached,
            ..game::ResultGraphs::EMPTY
        })
    }

    /// The page's graphs reach the screen with its lines; a page with new
    /// lines and the same graphs (a settings change) draws them again; a
    /// page without (a refused view) draws none.
    #[test]
    fn end_pages_reach_the_screen_with_their_graphs() {
        let (link, drew, _, _) = ends();
        let g = graphs(7);
        let last = |line: &str| {
            let drew = Arc::clone(&drew);
            let line = line.to_owned();
            move || drew.lock().unwrap().last().is_some_and(|d| d.0 == line)
        };
        link.show_end(page("A", Some(&g)));
        assert!(wait_until(last("A")));
        link.show_end(page("B", Some(&g)));
        assert!(wait_until(last("B")));
        link.show_end(page("C", None));
        assert!(wait_until(last("C")));
        let drew = drew.lock().unwrap().clone();
        let by = |line: &str| drew.iter().find(|d| d.0 == line).unwrap().1.clone();
        assert!(Arc::ptr_eq(&by("A").unwrap(), &g));
        assert!(Arc::ptr_eq(&by("B").unwrap(), &g));
        assert!(by("C").is_none());
        link.exit();
    }

    /// While the render thread is stuck in a frame, pages sent meanwhile
    /// replace each other in the slot: each replaced one is dropped at
    /// once, so one waits whatever the stall; once the frame ends, the
    /// newest is drawn, and the one drawn before is dropped.
    #[test]
    fn end_pages_coalesce_while_the_render_thread_stalls() {
        let (link, drew, hold, release) = ends();
        let first = graphs(1);
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("first", Some(&first)));
        // Held inside the first page's frame.
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        let sent: Vec<_> = (2..=6).map(graphs).collect();
        for (i, g) in sent.iter().enumerate() {
            link.show_end(page(&format!("page {i}"), Some(g)));
        }
        // The render thread holds the first page (and the screen's note
        // of it); the slot the newest.
        assert_eq!(Arc::strong_count(&first), 3);
        for g in &sent[..4] {
            assert_eq!(Arc::strong_count(g), 1);
        }
        assert_eq!(Arc::strong_count(&sent[4]), 2);
        release.send(()).unwrap();
        assert!(wait_until(|| {
            drew.lock().unwrap().last().is_some_and(|d| d.0 == "page 4")
        }));
        let drew_lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
        assert!(
            drew_lines.iter().all(|l| l == "first" || l == "page 4"),
            "{drew_lines:?}"
        );
        // The page drawn before went with the newer one taken: only the
        // screen's note of it is left.
        assert_eq!(Arc::strong_count(&first), 2);
        link.exit();
    }

    /// Left for a retry (a preparation follows) or for a menu screen
    /// (selection, the practice page): once the rig clears the slot, a
    /// page still waiting in it is never drawn, nor the page drawn before,
    /// whether the render thread was stuck in that page's frame or not.
    #[test]
    fn a_left_end_screen_draws_no_stale_page() {
        for (stalled, retry) in [(true, true), (true, false), (false, true), (false, false)] {
            let (link, drew, hold, release) = ends();
            let old = graphs(1);
            hold.store(stalled, Ordering::SeqCst);
            link.show_end(page("old", Some(&old)));
            assert!(wait_until(|| !drew.lock().unwrap().is_empty()));
            let waiting = graphs(2);
            if stalled {
                // A settings change while the frame is stuck: its page waits.
                link.show_end(page("waiting", Some(&waiting)));
                assert_eq!(Arc::strong_count(&waiting), 2);
            }
            // What the rig does on leaving: the slot is emptied first.
            link.clear_end();
            assert_eq!(Arc::strong_count(&waiting), 1);
            if retry {
                if stalled {
                    release.send(()).unwrap();
                }
                assert!(link.prepare().is_ok());
            } else {
                link.send(RenderMsg::Menu(Box::default()));
                if stalled {
                    release.send(()).unwrap();
                }
                assert!(wait_until(|| {
                    drew.lock().unwrap().iter().any(|d| d.0 == "<menu>")
                }));
            }
            let ends = |drew: &[Drew]| drew.iter().filter(|d| d.0 != "<menu>").count();
            let before = ends(&drew.lock().unwrap());
            std::thread::sleep(Duration::from_millis(100));
            let lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
            assert!(!lines.iter().any(|l| l == "waiting"), "{lines:?}");
            // No end frame once the screen was left.
            assert_eq!(ends(&drew.lock().unwrap()), before, "{lines:?}");
            if stalled {
                // Only the stuck frame of the old page.
                assert_eq!(before, 1, "{lines:?}");
            }
            if let Some(m) = lines.iter().position(|l| l == "<menu>") {
                assert!(lines[m..].iter().all(|l| l == "<menu>"), "{lines:?}");
            }
            // The render thread let go of the old page too: only the
            // screen's notes of it are left.
            let notes = drew
                .lock()
                .unwrap()
                .iter()
                .filter(|d| d.1.as_ref().is_some_and(|g| Arc::ptr_eq(g, &old)))
                .count();
            if stalled {
                assert_eq!(Arc::strong_count(&old), 1 + notes, "{retry}");
            }
            link.exit();
        }
    }
    /// A script's `drawn` step: the wait ends once the page shown last has
    /// been presented, and gives up at its bound while the render thread
    /// is stuck before drawing it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn waiting_for_the_end_page_ends_when_drawn_or_at_its_bound() {
        let (link, drew, hold, release) = ends();
        link.show_end(page("first", None));
        assert!(link.wait_end_drawn(Duration::from_secs(10)));
        assert!(!drew.lock().unwrap().is_empty());
        // Stuck in the next page's frame: the page after it is not drawn.
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("stuck", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("waiting", None));
        let began = Instant::now();
        assert!(!link.wait_end_drawn(Duration::from_millis(200)));
        assert!(began.elapsed() >= Duration::from_millis(200));
        release.send(()).unwrap();
        assert!(link.wait_end_drawn(Duration::from_secs(10)));
        assert_eq!(drew.lock().unwrap().last().unwrap().0, "waiting");
        link.exit();
    }

    /// Two pages coalesced: the first signal took and drew the newer page,
    /// and the second signal is still pending when the end screen is left.
    /// Neither the pending signal nor further frames draw that page again;
    /// left with no message pending, the page drawn last stops too.
    #[test]
    fn a_cleared_page_is_not_drawn_again() {
        let (link, drew, hold, release) = ends();
        let count = |line: &str| drew.lock().unwrap().iter().filter(|d| d.0 == line).count();
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("page 1", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("page 2", None));
        link.show_end(page("page 3", None));
        // The next end frame, page 3's for the signal of page 2, holds too.
        hold.store(true, Ordering::SeqCst);
        release.send(()).unwrap();
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        assert_eq!(count("page 3"), 1);
        // Page 3's own signal is pending.
        link.clear_end();
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(count("page 3"), 1, "{:?}", drew.lock().unwrap());
        assert_eq!(count("page 2"), 0);

        // Nothing pending: the page drawn last keeps being drawn until the
        // end screen is left, then stops.
        let (link, drew, _, _) = ends();
        let count = |line: &str| drew.lock().unwrap().iter().filter(|d| d.0 == line).count();
        link.show_end(page("shown", None));
        assert!(wait_until(|| count("shown") >= 3));
        link.clear_end();
        std::thread::sleep(Duration::from_millis(50));
        let after = count("shown");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(count("shown"), after);
        link.exit();
    }

    /// A retry in the production order: the rig leaves the end screen and
    /// then prepares the screen ([`crate::play::prepare_render`]), while
    /// the render thread is stuck in an end frame with a newer page
    /// waiting; the preparation's reply comes without that page drawn.
    #[test]
    fn a_retry_leaves_the_end_screen_before_preparing() {
        let (link, drew, hold, release) = ends();
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("shown", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("waiting", None));
        let mut end_page = Some(super::EndPage::default());
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
        });
        assert!(crate::play::prepare_render(&mut end_page, &link).is_ok());
        resume.join().unwrap();
        assert_eq!(end_page, None);
        std::thread::sleep(Duration::from_millis(100));
        let lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
        assert_eq!(lines, ["shown"]);
        link.exit();
    }

    /// A screen paced as a swap chain's waitable object with one frame of
    /// latency: a wait takes the one grant, a frame drawn gives it back,
    /// and with none a wait times out (after 5 ms). While `hold` is set,
    /// the wait that takes the grant waits for `release` once.
    struct Paced {
        drew: Arc<std::sync::Mutex<Vec<String>>>,
        grants: AtomicU32,
        hold: Arc<std::sync::atomic::AtomicBool>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Paced {
        fn drawn(&self, what: String) {
            self.drew.lock().unwrap().push(what);
            self.grants.store(1, Ordering::SeqCst);
        }
    }

    impl Screen for Paced {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            let took = self
                .grants
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |g| g.checked_sub(1));
            if took.is_err() {
                std::thread::sleep(Duration::from_millis(5));
                return false;
            }
            if self.hold.swap(false, Ordering::SeqCst) {
                let _ = self.release.recv_timeout(Duration::from_secs(10));
            }
            true
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.drawn("<play>".to_owned());
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            panic!("an end page is drawn with its graphs")
        }
        fn end(&mut self, page: &super::EndPage) -> Result<bool, RenderError> {
            self.drawn(page.lines.first().cloned().unwrap_or_default());
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.drawn("<menu>".to_owned());
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread over a [`Paced`] screen, the end page `result`
    /// drawn and the thread held in the wait for its next frame: the link,
    /// what it drew and the release.
    fn held_in_a_wait() -> (
        super::RenderLink,
        Arc<std::sync::Mutex<Vec<String>>>,
        std::sync::mpsc::Sender<()>,
    ) {
        let drew = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, released) = std::sync::mpsc::channel();
        let mut released = Some(released);
        let (noted, held) = (Arc::clone(&drew), Arc::clone(&hold));
        let make: MakeScreen = Box::new(move || {
            let release = released.take().expect("built once");
            Ok(Box::new(Paced {
                drew: Arc::clone(&noted),
                grants: AtomicU32::new(1),
                hold: Arc::clone(&held),
                release,
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.show_end(page("result", None));
        assert!(drawn(&drew, "result"));
        hold.store(true, Ordering::SeqCst);
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        (link, drew, release)
    }

    /// Whether `drew` noted `what` within the bound.
    fn drawn(drew: &std::sync::Mutex<Vec<String>>, what: &str) -> bool {
        wait_until(|| drew.lock().unwrap().iter().any(|d| d == what))
    }

    /// Selection or the practice page entered while the render thread
    /// waits for a frame of the end screen, in the rig's order: the end
    /// screen is left ([`super::RenderLink::clear_end`]), then the page is
    /// sent. The frame the wait granted goes to the page instead of being
    /// lost with the end page, so on a screen paced by one frame of
    /// latency the page is drawn, and no end frame after it.
    #[test]
    fn a_menu_entered_during_an_end_frame_wait_is_drawn() {
        let (link, drew, release) = held_in_a_wait();
        link.clear_end();
        link.send(RenderMsg::Menu(Box::default()));
        release.send(()).unwrap();
        assert!(drawn(&drew, "<menu>"), "{:?}", drew.lock().unwrap());
        let drew = drew.lock().unwrap().clone();
        let menu = drew.iter().position(|d| d == "<menu>").unwrap();
        assert!(drew[menu..].iter().all(|d| d != "result"), "{drew:?}");
        link.exit();
    }

    /// [`a_menu_entered_during_an_end_frame_wait_is_drawn`] for a retry:
    /// the end screen is left as the screen is prepared
    /// ([`crate::play::prepare_render`]), and the next play's frames are
    /// drawn.
    #[test]
    fn a_retry_during_an_end_frame_wait_draws_its_play() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let (link, drew, release) = held_in_a_wait();
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
        });
        let mut end_page = Some(super::EndPage::default());
        assert!(crate::play::prepare_render(&mut end_page, &link).is_ok());
        resume.join().unwrap();
        let clock = QpcClock::start();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        assert!(drawn(&drew, "<play>"), "{:?}", drew.lock().unwrap());
        link.exit();
    }

    /// Publishes coalesce: one wake waits however many sizes are
    /// published, and the newest is read. Reading the size between frames
    /// while that wake is still in the channel takes nothing: a publish
    /// after the read sends no second wake. Only taking the wake re-arms
    /// it, and the size read after that is the newest. Before a render
    /// thread follows, a publish marks no wake, so the first publish after
    /// the sender is set sends one; a wake sent between the sender's
    /// registration and the `Follow` being taken stays the only one, since
    /// taking `Follow` takes no wake. With the read clearing the flag, the
    /// publish after it would send a second wake; with a publish marking a
    /// wake before a sender exists, the first publish after it would send
    /// none; with `Follow` taking the wake, the publish after it would send
    /// a second one.
    #[test]
    fn extent_publishes_coalesce() {
        let extent = super::Extent::default();
        assert_eq!(extent.latest(), None);
        // No sender yet: nothing is marked.
        extent.publish(640, 480);
        assert_eq!(extent.latest(), Some((640, 480)));
        // `RenderLink::follow`: the sender is set, then `Follow` is sent;
        // a publish lands before the render thread takes `Follow`.
        let (tx, rx) = std::sync::mpsc::channel();
        *extent.wake.lock().unwrap() = Some(tx);
        extent.publish(800, 600);
        assert!(matches!(rx.try_recv(), Ok(RenderMsg::Resize)));
        // `Follow` is taken, then a frame reads the size while the wake
        // waits, then more publishes.
        assert_eq!(extent.latest(), Some((800, 600)));
        extent.publish(0, 0);
        extent.publish(2_560, 1_080);
        assert!(rx.try_recv().is_err(), "one wake for three publishes");
        // The render thread takes the wake, then reads the newest.
        extent.acknowledge();
        assert_eq!(extent.latest(), Some((2_560, 1_080)));
        extent.publish(u32::MAX, 1);
        assert!(matches!(rx.try_recv(), Ok(RenderMsg::Resize)));
        assert!(rx.try_recv().is_err());
        extent.acknowledge();
        assert_eq!(extent.latest(), Some((u32::MAX, 1)));
    }

    /// The sizes applied to a [`Resizing`] screen, each with the screen's
    /// `id`.
    type Applied = Arc<std::sync::Mutex<Vec<(u32, u32, u32)>>>;

    /// A render thread with nothing drawn (no page), so only a message
    /// wakes it, built on a [`Resizing`] screen; with the sizes applied.
    fn resizing_thread() -> (super::RenderLink, Applied) {
        let resized = Arc::new(std::sync::Mutex::new(Vec::new()));
        let r = Arc::clone(&resized);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Resizing {
                id: 1,
                size: (1_920, 1_080),
                resized: Arc::clone(&r),
                frames: Arc::new(std::sync::Mutex::new(Vec::new())),
                waited_minimized: Arc::new(AtomicU32::new(0)),
                fail: Arc::new(AtomicBool::new(false)),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        (link, resized)
    }

    /// Publishes racing each registration: a thread publishes on a fresh
    /// slot, released with the registration by a barrier, so publishes
    /// land between the sender's registration and its `Follow`. Once every
    /// message sent is taken, only a wake applies a size published after,
    /// so the slot's flag was left clear. Were the lock let go before the
    /// `Follow` is sent, a `Resize` could reach the render thread before it
    /// follows the slot, be left unacknowledged, and leave the flag set, so
    /// that later publishes would wake nothing.
    #[test]
    fn publishes_during_registration_still_wake_the_render_thread() {
        let (link, resized) = resizing_thread();
        let last = || resized.lock().unwrap().last().copied();
        for round in 1..=100 {
            let extent = Arc::new(super::Extent::default());
            let start = Arc::new(std::sync::Barrier::new(2));
            let (published, go) = (Arc::clone(&extent), Arc::clone(&start));
            let publisher = std::thread::spawn(move || {
                go.wait();
                for w in 1..=200 {
                    published.publish(w, 1);
                }
            });
            start.wait();
            link.follow(&extent);
            publisher.join().unwrap();
            link.end_loading().unwrap();
            extent.publish(4_096, round);
            assert!(
                wait_until(|| last() == Some((1, 4_096, round))),
                "{round}: {:?}",
                last()
            );
        }
        link.exit();
    }

    /// A slot followed again by a new render thread: the wake published
    /// after the first thread ended was sent nowhere, and its flag does
    /// not keep the new thread from being woken.
    #[test]
    fn a_new_render_thread_is_woken_after_the_last_ended() {
        let extent = Arc::new(super::Extent::default());
        let (first, _) = resizing_thread();
        first.follow(&extent);
        first.end_loading().unwrap();
        assert_eq!(first.exit(), super::RenderEnd::Joined);
        extent.publish(800, 600);
        let (second, resized) = resizing_thread();
        let last = || resized.lock().unwrap().last().copied();
        second.follow(&extent);
        assert!(wait_until(|| last() == Some((1, 800, 600))), "{:?}", last());
        second.end_loading().unwrap();
        extent.publish(1_280, 720);
        assert!(
            wait_until(|| last() == Some((1, 1_280, 720))),
            "{:?}",
            last()
        );
        second.exit();
    }

    /// Following `a`, then `b`, then `a` again: `a`'s wake, sent after
    /// `b`'s `Follow`, is taken while `b` is followed, so it acknowledges
    /// `b`; following `a` again clears the flag that wake left, and `a`'s
    /// later publishes wake the render thread.
    #[test]
    fn a_slot_followed_again_is_woken() {
        let (link, resized) = resizing_thread();
        let last = || resized.lock().unwrap().last().copied();
        let (a, b) = (
            Arc::new(super::Extent::default()),
            Arc::new(super::Extent::default()),
        );
        link.follow(&a);
        link.follow(&b);
        a.publish(800, 600);
        link.follow(&a);
        assert!(wait_until(|| last() == Some((1, 800, 600))), "{:?}", last());
        link.end_loading().unwrap();
        a.publish(1_280, 720);
        assert!(
            wait_until(|| last() == Some((1, 1_280, 720))),
            "{:?}",
            last()
        );
        link.exit();
    }

    /// A screen paced as a swap chain is, each the `id`th built, noting
    /// every size applied to it and every menu frame with whether it was
    /// minimized, and how often it was waited on while minimized. Its menu
    /// frame fails once when `fail` is set.
    struct Resizing {
        id: u32,
        size: (u32, u32),
        resized: Arc<std::sync::Mutex<Vec<(u32, u32, u32)>>>,
        frames: Arc<std::sync::Mutex<Vec<bool>>>,
        waited_minimized: Arc<AtomicU32>,
        fail: Arc<AtomicBool>,
    }

    impl Screen for Resizing {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            if self.minimized() {
                self.waited_minimized.fetch_add(1, Ordering::SeqCst);
            }
            true
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(RenderError::SurfaceLost);
            }
            let minimized = self.minimized();
            self.frames.lock().unwrap().push(minimized);
            Ok(timing(!minimized))
        }
        fn resize(&mut self, width: u32, height: u32) {
            self.resized.lock().unwrap().push((self.id, width, height));
            self.size = (width, height);
        }
        fn minimized(&self) -> bool {
            self.size.0 == 0 || self.size.1 == 0
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread follows the window's size: the size published
    /// before it followed is applied, then each newest one, each once;
    /// minimized, the select page is drawn on unpaced and unpresented and
    /// a preparation is answered, with no wait on the screen; after the
    /// burst the newest size is the one applied, and the page, sent again
    /// since a preparation stops it, is drawn presented; and a screen built
    /// again takes the size afresh. With the size applied only on a
    /// message, the first size would never reach the screen; with the
    /// pacing kept while minimized, the screen would be waited on.
    #[test]
    fn the_render_thread_follows_the_newest_size() {
        let resized = Arc::new(std::sync::Mutex::new(Vec::new()));
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let waited_minimized = Arc::new(AtomicU32::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let builds = Arc::new(AtomicU32::new(0));
        let (r, f, w, x, b) = (
            Arc::clone(&resized),
            Arc::clone(&frames),
            Arc::clone(&waited_minimized),
            Arc::clone(&fail),
            Arc::clone(&builds),
        );
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Resizing {
                id: b.fetch_add(1, Ordering::SeqCst) + 1,
                size: (1_920, 1_080),
                resized: Arc::clone(&r),
                frames: Arc::clone(&f),
                waited_minimized: Arc::clone(&w),
                fail: Arc::clone(&x),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let last = || resized.lock().unwrap().last().copied();
        let extent = Arc::new(super::Extent::default());
        extent.publish(800, 600);
        link.follow(&extent);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| last() == Some((1, 800, 600))));
        extent.publish(0, 0);
        assert!(wait_until(|| last() == Some((1, 0, 0))));
        let before = frames.lock().unwrap().len();
        assert!(wait_until(|| frames.lock().unwrap().len() >= before + 3));
        assert!(frames.lock().unwrap().last() == Some(&true));
        assert!(link.prepare().is_ok());
        // A preparation stops the select screen (`after_preparing`): the
        // page is sent again, as selection sends it after a load.
        link.send(RenderMsg::Menu(Box::default()));
        assert_eq!(waited_minimized.load(Ordering::SeqCst), 0);
        for size in [(1_024, 768), (1_280, 720), (2_560, 1_080)] {
            extent.publish(size.0, size.1);
        }
        assert!(wait_until(|| last() == Some((1, 2_560, 1_080))));
        let before = frames.lock().unwrap().len();
        assert!(wait_until(|| frames.lock().unwrap().len() > before));
        assert!(frames.lock().unwrap().last() == Some(&false));
        fail.store(true, Ordering::SeqCst);
        assert!(wait_until(|| link.failed()));
        assert!(link.prepare().is_ok());
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| last() == Some((2, 2_560, 1_080))));
        link.exit();
        let resized = resized.lock().unwrap().clone();
        // Each size applied once, in order.
        assert!(resized.windows(2).all(|p| p[0] != p[1]), "{resized:?}");
        assert_eq!(resized.first(), Some(&(1, 800, 600)));
        assert_eq!(resized.iter().filter(|r| r.0 == 2).count(), 1);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// A real declarative bundle with nothing to draw: no image, no
    /// element, its lanes laid out for the right scratch side, which no
    /// attempt of the simple skin's has.
    fn bundle() -> Arc<render::SkinBundle> {
        use skin_model::{Anchor, AtlasLimits, Fit, Region, RegionDef, Sheet, SkinDef, Variant};
        let sheet = Sheet::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let skin = skin_model::Skin::new(SkinDef {
            fit: Fit::Fixed,
            regions: [RegionDef {
                anchor: Anchor::TopLeft,
                optional: false,
            }; Region::COUNT],
            whens: Vec::new(),
            sheet,
            lanes: None,
            elements: Vec::new(),
            widgets: skin_model::WidgetDefs::default(),
            bomb_up_to: None,
        })
        .unwrap();
        let limits = AtlasLimits {
            max_side: 256,
            max_layers: 4,
            reserved_layers: 1,
        };
        let atlas = skin.atlas(Variant::Lo, limits).unwrap();
        let clips = render::ClipTable::new(Vec::new()).unwrap();
        Arc::new(
            render::SkinBundle::new(Arc::new(skin), atlas, clips, skin_model::Side::Right).unwrap(),
        )
    }

    /// The attempt of `chart`'s whole play with the title "T".
    fn titled(chart: &Chart) -> AttemptContext {
        let labels = game::ChartLabels {
            title: "T".to_owned(),
            ..game::ChartLabels::default()
        };
        let attempt = game::Attempt::new(
            1,
            &rules::tapestry(),
            TimeUs(1_000_000),
            None,
            game::NoteEnd::Play.of(chart),
            &labels,
        );
        AttemptContext::new(attempt, crate::attempt_context::SIMPLE_SCRATCH)
    }

    /// What an [`Installer`] was asked, in order.
    type Log = Arc<std::sync::Mutex<Vec<String>>>;

    /// A screen that stages a declarative skin's installation as the
    /// renderer does, noting each step with its device: a preparation
    /// answered (`prepare` with its request, the context's scratch side
    /// and the attempt's title), a commit, a discard and a restore. While
    /// `hold` is set, its preparations do not answer. Its menu frames fail
    /// while `fail` is set; it counts its loading frames.
    struct Installer {
        device: u64,
        hold: Arc<AtomicBool>,
        fail: Arc<AtomicBool>,
        loading: Arc<AtomicU32>,
        log: Log,
        restored: Arc<std::sync::Mutex<Vec<Arc<render::SkinBundle>>>>,
    }

    impl Installer {
        fn note(&self, step: &str) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} {step}", self.device));
        }
    }

    impl Screen for Installer {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn prepare_attempt(
            &mut self,
            a: &render::AttemptInput<'_>,
            skin: Option<&Arc<render::SkinBundle>>,
            request: u64,
        ) -> Option<Result<Preparation, RenderError>> {
            if self.hold.load(Ordering::Acquire) {
                return None;
            }
            assert!(skin.is_some());
            self.note(&format!(
                "prepare {request} {:?} {}",
                a.context.scratch, a.text.title
            ));
            Some(Ok(Preparation::Ready))
        }
        fn device(&self) -> u64 {
            self.device
        }
        fn commit(&mut self, token: render::PreparedToken) -> Result<(), render::CommitError> {
            if token.device != self.device {
                return Err(render::CommitError::Device);
            }
            self.note(&format!("commit {}", token.request));
            Ok(())
        }
        fn discard(&mut self) {
            self.note("discard");
        }
        fn restore(&mut self, bundle: &Arc<render::SkinBundle>) -> Result<(), RenderError> {
            self.note("restore");
            self.restored.lock().unwrap().push(Arc::clone(bundle));
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            if self.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            Ok(timing(true))
        }
        fn loading(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _update: Option<&LoadingUpdate>,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.loading.fetch_add(1, Ordering::AcqRel);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The shared state of the [`Installer`]s a render thread builds,
    /// their devices numbered from 1.
    #[derive(Clone, Default)]
    struct Installers {
        builds: Arc<AtomicU32>,
        hold: Arc<AtomicBool>,
        fail: Arc<AtomicBool>,
        loading: Arc<AtomicU32>,
        log: Log,
        restored: Arc<std::sync::Mutex<Vec<Arc<render::SkinBundle>>>>,
    }

    impl Installers {
        /// A render thread on these installers with `chart` loaded and
        /// `skin` chosen for its attempts.
        fn spawn(&self, chart: &Arc<Chart>, skin: &Arc<render::SkinBundle>) -> super::RenderLink {
            let shared = self.clone();
            let make: MakeScreen = Box::new(move || {
                let device = u64::from(shared.builds.fetch_add(1, Ordering::SeqCst) + 1);
                Ok(Box::new(Installer {
                    device,
                    hold: Arc::clone(&shared.hold),
                    fail: Arc::clone(&shared.fail),
                    loading: Arc::clone(&shared.loading),
                    log: Arc::clone(&shared.log),
                    restored: Arc::clone(&shared.restored),
                }) as Box<dyn Screen>)
            });
            let link = spawn_empty(make);
            link.load(Arc::clone(chart), None).unwrap();
            link.select(Some(Arc::clone(skin)));
            link
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    /// A ready preparation installs only by its own request's commit:
    /// prepared with the chosen bundle's scratch side and the attempt's
    /// labels, it is committed; one superseded by the next request is
    /// discarded, and a commit naming it changes nothing and leaves the
    /// newer one to commit; one discarded (refused, cancelled) cannot be
    /// committed.
    #[test]
    fn an_attempt_installs_only_by_its_own_commit() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let link = screens.spawn(&chart, &skin);
        let ready = |link: &super::RenderLink| link.prepare_attempt(titled(&chart), || false);

        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        // Request 2, superseded by 3 before its commit.
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        link.send(RenderMsg::Commit { request: 2, reply });
        assert_eq!(
            rx.recv().unwrap(),
            Err(render::CommitError::Request {
                staged: 3,
                asked: 2
            })
        );
        link.commit_attempt().unwrap();
        // Request 4, discarded.
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.discard_attempt();
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 prepare 1 Right T",
                "1 commit 1",
                "1 prepare 2 Right T",
                "1 discard",
                "1 prepare 3 Right T",
                "1 commit 3",
                "1 prepare 4 Right T",
                "1 discard",
            ]
        );
    }

    /// While a preparation is held unanswered, loading frames go on being
    /// presented; released, it answers. A preparation cancelled while held,
    /// or past the answer's bound, is discarded: released later, it never
    /// answers or installs, and its commit is refused.
    #[test]
    fn loading_frames_go_on_while_a_preparation_is_held() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let mut link = screens.spawn(&chart, &skin);
        let frames = || screens.loading.load(Ordering::Acquire);
        link.loading().show(one_line("loading"));
        assert!(wait_until(|| frames() > 0));

        // Held until three more loading frames were presented.
        screens.hold.store(true, Ordering::Release);
        let from = frames();
        let ready = link.prepare_attempt(titled(&chart), || {
            if frames() >= from + 3 {
                screens.hold.store(false, Ordering::Release);
            }
            false
        });
        assert_eq!(ready.unwrap(), Preparation::Ready);
        assert!(frames() >= from + 3);
        link.commit_attempt().unwrap();

        // Cancelled while held, then released: the late answer never
        // comes, and nothing commits.
        screens.hold.store(true, Ordering::Release);
        let from = frames();
        let cancelled = link.prepare_attempt(titled(&chart), || frames() >= from + 3);
        assert_eq!(cancelled.unwrap(), Preparation::Cancelled);
        screens.hold.store(false, Ordering::Release);
        let from = frames();
        assert!(wait_until(|| frames() >= from + 3));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));

        // Past the bound while held: the same.
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        screens.hold.store(true, Ordering::Release);
        let late = link.prepare_attempt(titled(&chart), || false);
        assert!(matches!(late, Err(ScreenError::NoAnswer)), "{late:?}");
        screens.hold.store(false, Ordering::Release);
        let from = frames();
        assert!(wait_until(|| frames() >= from + 3));
        link.set_timeouts(Duration::from_secs(10), Duration::from_secs(5));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 prepare 1 Right T",
                "1 commit 1",
                "1 discard",
                "1 discard"
            ]
        );
    }

    /// A screen built again after a failure restores the bundle committed
    /// last, the very one, before anything else; a preparation answered on
    /// the failed screen is refused at its commit by its device, and the
    /// next attempt prepares and commits on the new screen.
    #[test]
    fn a_screen_built_again_restores_the_committed_bundle() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let link = screens.spawn(&chart, &skin);
        let ready = |link: &super::RenderLink| link.prepare_attempt(titled(&chart), || false);
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);

        screens.fail.store(true, Ordering::Release);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        screens.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        assert_eq!(screens.builds.load(Ordering::SeqCst), 2);
        let restored = screens.restored.lock().unwrap().clone();
        assert_eq!(restored.len(), 1);
        assert!(Arc::ptr_eq(&restored[0], &skin));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));

        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 prepare 1 Right T",
                "1 commit 1",
                "1 prepare 2 Right T",
                "2 restore",
                "2 discard",
                "2 prepare 3 Right T",
                "2 commit 3",
            ]
        );
    }

    /// Each frame's effect sequence numbers and play phase.
    type Adaptations = Arc<std::sync::Mutex<Vec<(Vec<u64>, bool)>>>;

    /// A screen that builds every play frame through the declarative
    /// skin's frame adapter, noting the effects' sequence numbers and the
    /// play phase it reads.
    struct Adapted {
        seen: Adaptations,
    }

    impl Screen for Adapted {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            let frame = render::skin_frame(f);
            assert!(std::ptr::eq(frame.tally, &f.view.tally));
            let seqs = frame.effects.iter().map(|e| e.seq).collect();
            self.seen.lock().unwrap().push((seqs, frame.playing));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The effects reach the declarative skin's frames in the order the
    /// logic step queued them, across drains of a buffer of 2: every
    /// effect once, none reordered, each frame at most a buffer's worth,
    /// with the play phase of the frame's own view.
    #[test]
    fn effects_reach_the_skin_frames_in_order_across_drains() {
        use game::DeviceSettings;
        use input::DeviceKind;

        let buffer = Vec::<eval::JudgeEvent>::with_capacity(2).capacity();
        let mut builder = ChartBuilder::new(120_000);
        for k in 0..6 {
            builder = builder.tap(Lane::new(1).unwrap(), TimeUs(1_000_000 + k * 200_000));
        }
        let chart = builder.build().unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (mut writer, reader) = input::source_queue(32);
        for e in input::autoplay(&chart, &device.sources[0]).unwrap() {
            writer.push(e).unwrap();
        }
        writer.publish(TimeUs(3_000_000));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 32,
                effects: 64,
            },
            Vec::new(),
        )
        .unwrap();
        logic.advance_to(TimeUs(3_000_001));
        let queued: Vec<u64> = logic.events().iter().map(|e| e.seq).collect();
        assert!(queued.len() > 2 * buffer);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Adapted {
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            View {
                effects: buffer,
                ..view()
            },
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        let drawn = || -> usize { seen.lock().unwrap().iter().map(|(s, _)| s.len()).sum() };
        assert!(wait_until(|| drawn() >= queued.len()));
        let _ = link.finish(clock.now());
        link.exit();
        let frames = seen.lock().unwrap().clone();
        assert!(frames.iter().all(|(s, _)| s.len() <= buffer), "{frames:?}");
        assert!(frames.iter().filter(|(s, _)| !s.is_empty()).count() > 1);
        let order: Vec<u64> = frames.iter().flat_map(|(s, _)| s.iter().copied()).collect();
        assert_eq!(order, queued);
        let playing = logic.view().playing;
        assert!(frames.iter().all(|&(_, p)| p == playing));
    }
}
