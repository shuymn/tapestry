//! The loading state before a play and the opening after it (design.md
//! "Play Lifecycle": the loading state before a play, tuning in it, the
//! opening after it, its exits, the start gate).
//!
//! [`load_and_open`] begins the attempt's preparation once the chart is
//! loaded, then waits in the loading state until every condition holds:
//!
//! - **the minimum**, the larger of 1 s and the play skin's `loadend`
//!   ([`loading_minimum`]), counted from the render side's acknowledgement
//!   that it presented the first frame of this loading lifetime; an
//!   acknowledgement of another lifetime is ignored, and the first one of
//!   this lifetime is kept;
//! - **the tuning controls** quiet for [`QUIET`] since the last activity,
//!   nothing held and no overflow, and **focus**: one decision on one
//!   snapshot of the tuning ([`TuningSnapshot::quiet_at`]), which decides
//!   again on the opening's own snapshot and at its revalidation.
//!
//! Each slice of the wait polls the preparation while it is pending (an
//! answer is latched once and the answered handle never polled again),
//! rebuilds a failed screen within the same lifetime, and looks at the
//! threads, a refusal, a close and the quit key in that order. Then the
//! opening runs in the order of design.md "Play Lifecycle", opening after
//! the loading state:
//!
//! 1. a copy of the tuning ([`LoadingRig::tuning_snapshot`]), quiet at the
//!    time it is taken (else the wait goes on), becomes the attempt's
//!    starting settings ([`LoadingRig::apply_snapshot`]);
//! 2. the screen, the play's number and preset, a round's refit, the
//!    presentation's new epoch ([`LoadingRig::refresh_presentation`]) and
//!    audio;
//! 3. the preparation's key is built again: a moved key begins the
//!    preparation again, an unmoved one keeps the latched answer; then the
//!    final readiness, whose structural `Stale` begins it again (or, with
//!    the tuning moved meanwhile, waits again first) and whose unchanged
//!    key ends the run;
//! 4. a round's restore check and the look at a quit or a failure;
//! 5. the revalidation ([`LoadingRig::revalidate_and_close`]): tuning
//!    activity since step 1, or tuning no longer quiet, gives the number
//!    back and waits again, the preparation kept (a local retry); otherwise
//!    tuning acceptance closes;
//! 6. the commit; a minimum the commit lengthened (a pending skin's
//!    specialization declares its loading time only once committed) is
//!    waited out with acceptance reopened, then closed again on a fresh,
//!    quiet snapshot applied to the starting settings; the end of loading
//!    and the last look; then recording opens through the common play
//!    loop, once.
//!
//! A deferred opening reopens tuning acceptance. One deferred by a held
//! START or SELECT is asked again at the release; one deferred for focus
//! once focus is back, START and SELECT released and the tuning controls
//! quiet for [`QUIET`] from the return. Either closes acceptance again on
//! a fresh snapshot (one deferred for focus only when that snapshot is
//! quiet too), applied to the starting settings, and asks again with the
//! same candidate generation. Every exit before the commit except the
//! local retry discards the preparation; every exit that does not end the
//! run reopens tuning acceptance.

use bms_model::TimeUs;
use input::GateState;

use crate::attempt::{Preparation, PreparationRefusal};
use crate::live::{
    FirstOpening, InputWaitError, Leave, LiveError, LoadingGate, OpenPolicy, Opening, PREP_SLICE,
    Played, Rig, open_and_play, preflight, refused, threads_over,
};
use crate::prep::{PrepHandle, PrepKey, PrepPoll, Readiness};
use crate::session::{Command, Loaded, PlayStart, Settings};
use crate::tuning::Tuning;

/// The loading state's minimum when the play skin declares less, or
/// nothing.
pub const LOADING_FLOOR: TimeUs = TimeUs(1_000_000);

/// How long the tuning controls stay quiet before the loading state ends.
pub const QUIET: TimeUs = TimeUs(1_000_000);

/// The loading state's minimum for a play skin declaring `loadend`: the
/// larger of it and [`LOADING_FLOOR`].
#[must_use]
pub const fn loading_minimum(loadend: Option<TimeUs>) -> TimeUs {
    match loadend {
        Some(d) if d.0 > LOADING_FLOOR.0 => d,
        _ => LOADING_FLOOR,
    }
}

/// Where the minimum ends for a first frame presented at `ack`
/// ([`loading_minimum`]), saturating at the clock's end.
#[must_use]
pub const fn minimum_ends(ack: TimeUs, loadend: Option<TimeUs>) -> TimeUs {
    TimeUs(ack.0.saturating_add(loading_minimum(loadend).0))
}

/// Tuning acceptance's revision: it moves with every accepted press,
/// repeat and release of a held control, value change, overflow and focus
/// change, whatever their time stamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AcceptanceRevision {
    /// The window side's (settings keys, held set, focus).
    pub window: u64,
    /// The input side's START and SELECT.
    pub input: u64,
    /// The play adjustments the reducer accepted.
    pub reducer: u64,
}

/// A copy of the tuning taken under the tuning step lock: what the
/// loading state's conditions and the opening's revalidation read, as one
/// coherent observation of the window side, the input side and the
/// reducer. Taking it allocates nothing and calls nothing back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TuningSnapshot {
    /// The observation is coherent. An unstable one (a write in progress
    /// through every attempt to read it) is never quiet and never
    /// unchanged since another.
    pub stable: bool,
    /// The settings keys' values; `None` for a rig without them.
    pub values: Option<Tuning>,
    /// The acceptance revision.
    pub revision: AcceptanceRevision,
    /// A tuning control is held.
    pub held: bool,
    /// A press did not fit the held set: held until focus loss clears it.
    pub overflow: bool,
    /// The window has focus.
    pub focused: bool,
    /// The window's focus losses so far: the generation an opening out of
    /// this snapshot's close accepts.
    pub focus_losses: u64,
    /// The last activity on the rig's clock; `None` before any.
    pub last_activity: Option<TimeUs>,
}

impl TuningSnapshot {
    /// Nothing held, focused and no activity: a rig without tuning in its
    /// loading state.
    pub const IDLE: Self = Self {
        stable: true,
        values: None,
        revision: AcceptanceRevision {
            window: 0,
            input: 0,
            reducer: 0,
        },
        held: false,
        overflow: false,
        focused: true,
        focus_losses: 0,
        last_activity: None,
    };

    /// Whether the tuning lets the loading state end at `now`: a stable
    /// observation, focused, nothing held, no overflow, and [`QUIET`]
    /// since the last activity. The one decision of the wait, the
    /// opening's snapshot and its revalidation.
    #[must_use]
    pub const fn quiet_at(&self, now: TimeUs) -> bool {
        let quiet = match self.last_activity {
            Some(last) => now.0 >= last.0.saturating_add(QUIET.0),
            None => true,
        };
        quiet && self.stable && self.focused && !self.held && !self.overflow
    }

    /// Whether nothing changed since `earlier` was taken (both stable, the
    /// same acceptance revision) and the tuning is still quiet at `now`
    /// ([`quiet_at`](Self::quiet_at)).
    #[must_use]
    pub fn unchanged_since(&self, earlier: &Self, now: TimeUs) -> bool {
        earlier.stable && self.revision == earlier.revision && self.quiet_at(now)
    }
}

/// What the opening's revalidation found
/// ([`LoadingRig::revalidate_and_close`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Revalidated {
    /// Nothing changed: tuning acceptance is closed on both sides until
    /// candidate generation's recording opens, or a deferral or an exit
    /// reopens it.
    Closed,
    /// The tuning changed: acceptance stays open, the close reversed.
    Changed,
}

/// What the loading state needs of a rig (design.md "Play Lifecycle", the
/// loading state before a play). The defaults are a rig without a loading
/// state: no minimum, no tuning wait, the starting settings taken as
/// [`Rig::before_play`] gives them and the presentation begun as
/// [`Rig::begin_presentation`] does.
pub trait LoadingRig: Rig {
    /// The render side's loading lifetime the chart's load began, which
    /// the minimum is counted in.
    fn loading_lifetime(&self) -> u64 {
        0
    }
    /// The render side's latest acknowledgement of a lifetime's first
    /// presented frame: the lifetime it was drawn for and when it was
    /// presented. By default, one presented long before.
    fn loading_ack(&self) -> Option<(u64, TimeUs)> {
        Some((self.loading_lifetime(), TimeUs(i64::MIN)))
    }
    /// The loading time the play skin declares (`loadend`): the installed
    /// skin's, or a pending skin's base until its specialization commits.
    /// None by default.
    fn loadend(&self) -> Option<TimeUs> {
        None
    }
    /// A copy of the tuning, taken under the tuning step lock and with
    /// the reducer's count read after a fence. [`TuningSnapshot::IDLE`]
    /// by default.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    fn tuning_snapshot(&mut self) -> Result<TuningSnapshot, InputWaitError> {
        Ok(TuningSnapshot::IDLE)
    }
    /// Outside any lock: `snapshot` becomes the attempt's starting
    /// settings, written into `settings` and the play adjustments' options.
    /// By default, [`Rig::before_play`].
    fn apply_snapshot(&mut self, snapshot: &TuningSnapshot, settings: &mut Settings) {
        let _ = snapshot;
        self.before_play(settings);
    }
    /// The presentation takes the attempt `loaded` describes again under
    /// a new epoch, without entering it: holds, the selected cover and the
    /// dispatch context stay. `Ok(Some(_))` refuses the attempt. By
    /// default, [`Rig::begin_presentation`].
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    fn refresh_presentation(
        &mut self,
        loaded: &Loaded,
    ) -> Result<Option<PreparationRefusal>, InputWaitError> {
        self.begin_presentation(loaded)
    }
    /// Fences the input thread and closes tuning acceptance at the fence,
    /// then compares the tuning with `at_o1`, the opening's snapshot
    /// ([`TuningSnapshot::unchanged_since`] at the rig's time): unchanged,
    /// the window side closes too until candidate `generation`'s recording
    /// opens ([`Revalidated::Closed`]); changed, the close is reversed at
    /// the fence, so nothing observed since is discarded
    /// ([`Revalidated::Changed`]). Closed by default.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    fn revalidate_and_close(
        &mut self,
        at_o1: &TuningSnapshot,
        generation: u32,
    ) -> Result<Revalidated, InputWaitError> {
        let _ = (at_o1, generation);
        Ok(Revalidated::Closed)
    }
    /// After a deferred opening's release: closes tuning acceptance on
    /// both sides until candidate `generation`'s recording opens, and
    /// returns the tuning as it closed. By default,
    /// [`tuning_snapshot`](Self::tuning_snapshot).
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    fn close_acceptance(&mut self, generation: u32) -> Result<TuningSnapshot, InputWaitError> {
        let _ = generation;
        self.tuning_snapshot()
    }
    /// Reopens tuning acceptance on both sides. Nothing by default.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    fn reopen_acceptance(&mut self) -> Result<(), InputWaitError> {
        Ok(())
    }
    /// Whether a deferred opening waits on the tuning's quiet second from
    /// focus's return, not only on the release of START and SELECT: the
    /// last opening was deferred for focus (absent, or lost since the
    /// batch the input side sampled), or focus was lost since tuning
    /// acceptance last reopened. Read before the deferral reopens it and
    /// at every look of the wait. `false` by default.
    fn focus_deferred(&self) -> bool {
        false
    }
}

/// A preparation's answer, latched: only `Pending` is polled.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PrepState {
    Pending,
    Ready,
    Refused(PreparationRefusal),
    Cancelled,
}

impl PrepState {
    fn of(answer: Preparation) -> Self {
        match answer {
            Preparation::Ready => Self::Ready,
            Preparation::Cancelled => Self::Cancelled,
            Preparation::Refused(why) => Self::Refused(why),
        }
    }
}

/// The loading state's opener of a chart's first attempt
/// ([`load_and_open`]): the preparation begun at load-ready with its
/// latched answer, the loading lifetime and its first frame, and whether
/// tuning acceptance is closed.
#[derive(Debug)]
pub struct LoadingOpener {
    lifetime: u64,
    ack: Option<TimeUs>,
    handle: PrepHandle,
    latch: PrepState,
    /// A `Stale` final readiness found the key moved: the preparation
    /// begins again at the next key check.
    begin_again: bool,
    /// Tuning acceptance was closed by the opener and not reopened since.
    closed: bool,
}

impl LoadingOpener {
    /// Whether the minimum has passed at `now`: the lifetime's first frame
    /// acknowledged, and the loading time the play skin declares now
    /// counted from it.
    fn minimum_passed<R: LoadingRig>(&self, rig: &R, now: TimeUs) -> bool {
        self.ack
            .is_some_and(|at| now.0 >= minimum_ends(at, rig.loadend()).0)
    }
}

/// Plays a chart whose load is ready, its first attempt opened out of the
/// loading state: the attempt's preparation begins now, the loading state
/// waits and opens as [this module](self) describes, and from the
/// opening on the chart runs as [`crate::live::run_chart`] does, every
/// later attempt opened the standard way. Every return that does not end
/// the run reopens tuning acceptance.
///
/// # Errors
///
/// As [`crate::live::run_chart`]; a refused preparation, refit or restore
/// before the opening is [`LiveError::Preparation`] or
/// [`LiveError::Restore`] with its reason, and ends nothing.
pub fn load_and_open<R: LoadingRig>(
    loaded: &mut Loaded,
    rig: &mut R,
) -> Result<Leave, LiveError<R::ScreenError>> {
    // The attempt is prepared before its play begins: its best and target
    // are set first, and the begin reads no more under the same key.
    loaded.prepare_score();
    let key = loaded.prep_key();
    let handle = rig.begin_preparation(loaded, key);
    let mut opener = LoadingOpener {
        lifetime: rig.loading_lifetime(),
        ack: None,
        handle,
        latch: PrepState::Pending,
        begin_again: false,
        closed: false,
    };
    let left = crate::live::run_chart_with(loaded, rig, FirstOpening::Loading(&mut opener));
    // A play abandoned before it began took no number.
    loaded.end_ahead();
    let ended = match &left {
        Ok(Leave::Back) => false,
        Ok(Leave::Quit) => true,
        Err(e) => !matches!(
            e,
            LiveError::Preparation(_) | LiveError::Restore(_) | LiveError::Logic(_)
        ),
    };
    if opener.closed && !ended {
        rig.reopen_acceptance().map_err(LiveError::Input)?;
        // The reopening fences the input thread: a failure that came
        // meanwhile ends the run.
        threads_over(rig)?;
    }
    left
}

/// The chart's first attempt, opened out of the loading state, played
/// through [`open_and_play`]. `None` when it was left before recording
/// opened.
pub(crate) fn play_loading<R: LoadingRig>(
    loaded: &mut Loaded,
    rig: &mut R,
    opener: &mut LoadingOpener,
) -> Result<Option<Played>, LiveError<R::ScreenError>> {
    match ready_loading(loaded, rig, opener)? {
        Some(start) => open_and_play(loaded, rig, start, OpenPolicy::Loading(opener)),
        None => Ok(None),
    }
}

/// How the opening's steps before the commit ended ([`before_commit`]).
enum Before {
    /// Ready to commit, tuning acceptance closed.
    Ready,
    /// The tuning moved: wait again, the preparation kept.
    Retry,
    /// The player quit or closed, or the preparation was cancelled.
    Left,
}

/// The loading state's wait and the opening's steps up to recording's
/// opening: the play's start, ready to open; `None` when the player left
/// first, its number given back.
///
/// # Errors
///
/// What [`crate::live::play`] reports before recording opens; a number
/// already given is withdrawn.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn ready_loading<R: LoadingRig>(
    loaded: &mut Loaded,
    rig: &mut R,
    opener: &mut LoadingOpener,
) -> Result<Option<PlayStart>, LiveError<R::ScreenError>> {
    loop {
        let mut start = None;
        let before = before_commit(loaded, rig, opener, &mut start);
        let start = match (before, start) {
            (Ok(Before::Ready), Some(start)) => start,
            // A local retry keeps the preparation; the presentation's
            // epoch is named again at the next refresh.
            (Ok(Before::Retry), start) => {
                if let Some(start) = start {
                    rig.end_presentation();
                    loaded.withdraw(start);
                }
                continue;
            }
            (Ok(Before::Ready | Before::Left), start) => {
                dispose(loaded, rig, start);
                // The discard can block on a running job's thread: a
                // failure that came meanwhile ends the run.
                threads_over(rig)?;
                return Ok(None);
            }
            (Err(e), start) => {
                dispose(loaded, rig, start);
                return Err(refused(rig, e));
            }
        };
        // Committed from here on: the installation stays on every exit.
        let after = commit(loaded, rig, opener);
        return match after {
            Ok(true) => Ok(Some(start)),
            Ok(false) => {
                rig.end_presentation();
                loaded.withdraw(start);
                threads_over(rig)?;
                Ok(None)
            }
            Err(e) => {
                rig.end_presentation();
                loaded.withdraw(start);
                Err(e)
            }
        };
    }
}

/// An exit before the commit: the preparation discarded, a number given
/// back with the presentation's attempt.
fn dispose<R: Rig>(loaded: &mut Loaded, rig: &mut R, start: Option<PlayStart>) {
    rig.discard_attempt();
    if let Some(start) = start {
        rig.end_presentation();
        loaded.withdraw(start);
    }
}

/// The commit, the end of loading and the last look: `false` when the
/// player quit or closed before the end of loading or at the last look.
///
/// A pending play skin's specialization declares its loading time only
/// once committed in the lifetime (design.md "Rendering and Skins", the
/// pending presentation), so the minimum is looked at again after the
/// commit. While it still runs, tuning acceptance reopens and the loading
/// state waits as before the opening; then acceptance closes again on a
/// fresh copy, which must be quiet and past the minimum at one time
/// (else it reopens and waits on) and becomes the starting settings. The
/// commit is not taken again, and the closed gap is the end of loading
/// and the last look.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn commit<R: LoadingRig>(
    loaded: &mut Loaded,
    rig: &mut R,
    opener: &mut LoadingOpener,
) -> Result<bool, LiveError<R::ScreenError>> {
    rig.commit_attempt().map_err(LiveError::Screen)?;
    if !opener.minimum_passed(rig, rig.now()) {
        let generation = next_generation(rig.candidate());
        rig.reopen_acceptance().map_err(LiveError::Input)?;
        opener.closed = false;
        let snapshot = loop {
            if !wait_loading(rig, opener, true)? {
                return Ok(false);
            }
            let snapshot = rig.close_acceptance(generation).map_err(LiveError::Input)?;
            opener.closed = true;
            let now = rig.now();
            if opener.minimum_passed(rig, now) && snapshot.quiet_at(now) {
                break snapshot;
            }
            rig.reopen_acceptance().map_err(LiveError::Input)?;
            opener.closed = false;
        };
        rig.apply_snapshot(&snapshot, loaded.settings_mut());
    }
    rig.end_loading().map_err(LiveError::Screen)?;
    preflight(rig)?;
    Ok(!quit_look(rig))
}

/// Whether the player is leaving: a close, or a quit among the commands
/// pending, all of which are taken (a retry or suspend before recording
/// opens belongs to no play).
fn quit_look<R: Rig>(rig: &mut R) -> bool {
    let mut quit = rig.quitting();
    while let Some(c) = rig.take() {
        quit |= c == Command::Quit;
    }
    quit
}

/// One pass of the wait and the opening's steps before the commit; a
/// number given is put in `start`.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn before_commit<R: LoadingRig>(
    loaded: &mut Loaded,
    rig: &mut R,
    opener: &mut LoadingOpener,
    start: &mut Option<PlayStart>,
) -> Result<Before, LiveError<R::ScreenError>> {
    // The starting settings: a copy under the lock, decided quiet at its
    // own time (the wait's look may be stale by now), applied outside it.
    let at_o1 = loop {
        if !wait_loading(rig, opener, false)? {
            return Ok(Before::Left);
        }
        let at_o1 = rig.tuning_snapshot().map_err(LiveError::Input)?;
        if at_o1.quiet_at(rig.now()) {
            break at_o1;
        }
    };
    rig.apply_snapshot(&at_o1, loaded.settings_mut());
    rig.prepare_screen().map_err(LiveError::Screen)?;
    let begun = loaded.begin_play();
    let number = begun.attempt;
    *start = Some(begun);
    if let Err(unfit) = loaded.refit_practice() {
        return Err(LiveError::Preparation(PreparationRefusal {
            location: None,
            reason: unfit.to_string(),
        }));
    }
    if let Some(refusal) = rig.refresh_presentation(loaded).map_err(LiveError::Input)? {
        return Err(LiveError::Preparation(refusal));
    }
    rig.prepare_audio().map_err(LiveError::Audio)?;
    loop {
        // The key, built outside any lock: one moved since the
        // preparation began (a gauge resolved anew, a preset reloaded)
        // begins it again.
        let now = loaded.prep_key();
        if opener.begin_again || moved(&now, opener.handle.key()) {
            opener.handle = rig.begin_preparation(loaded, now);
            opener.latch = PrepState::Pending;
            opener.begin_again = false;
        }
        while opener.latch == PrepState::Pending {
            let deadline = std::time::Instant::now() + PREP_SLICE;
            match rig
                .poll_preparation(&mut opener.handle, deadline)
                .map_err(LiveError::Screen)?
            {
                PrepPoll::Pending => {}
                PrepPoll::Done(answer) => opener.latch = PrepState::of(answer),
            }
        }
        match &opener.latch {
            PrepState::Refused(why) => return Err(LiveError::Preparation(why.clone())),
            PrepState::Cancelled => {
                threads_over(rig)?;
                let _ = quit_look(rig);
                return Ok(Before::Left);
            }
            PrepState::Ready | PrepState::Pending => {}
        }
        match rig
            .ready_attempt(number, &opener.handle)
            .map_err(LiveError::Screen)?
        {
            Readiness::Ready => break,
            Readiness::Stale(stale) => {
                // What ends the run or the play comes first, as at the
                // commit; then an unchanged key ends the run untried.
                preflight(rig)?;
                if quit_look(rig) {
                    return Ok(Before::Left);
                }
                let now = loaded.prep_key().with_screen(stale.screen);
                if now == *opener.handle.key() {
                    return Err(LiveError::Screen(stale.lost));
                }
                opener.begin_again = true;
                // The tuning moved meanwhile: wait again before the new
                // preparation.
                let tuning = rig.tuning_snapshot().map_err(LiveError::Input)?;
                if !tuning.unchanged_since(&at_o1, rig.now()) {
                    return Ok(Before::Retry);
                }
            }
        }
    }
    if let Some(at) = loaded.restore_at() {
        rig.restore_ready(at).map_err(LiveError::Restore)?;
    }
    preflight(rig)?;
    if quit_look(rig) {
        return Ok(Before::Left);
    }
    match rig
        .revalidate_and_close(&at_o1, next_generation(rig.candidate()))
        .map_err(LiveError::Input)?
    {
        Revalidated::Closed => {
            opener.closed = true;
            Ok(Before::Ready)
        }
        Revalidated::Changed => Ok(Before::Retry),
    }
}

/// The candidate generation the attempt's opening asks for: after the
/// gate's last, as [`open_and_play`] numbers it.
fn next_generation(gate: GateState) -> u32 {
    gate.generation.wrapping_add(1).max(1)
}

/// Whether `now` differs from the key the preparation `was` begun with,
/// the screen's structural inputs aside: those are found at staging, and
/// a change of them is the final readiness's `Stale`.
fn moved(now: &PrepKey, was: &PrepKey) -> bool {
    let changes = now.changes(was);
    !crate::prep::Changes {
        screen: false,
        ..changes
    }
    .none()
}

/// The loading state's wait (design.md "Play Lifecycle", the loading state
/// before a play): `false` when the player left (a quit, a close, or the
/// preparation cancelled). `committed` is the wait after the commit, for a
/// committed specialization's longer minimum: a screen lost there is not
/// built again, since the replacement would hold no prepared attempt.
///
/// # Errors
///
/// A thread's failure, a refusal latched by the preparation, the screen's
/// failure at a poll or a rebuild (after the commit, any loss of it), and
/// the input thread's at a snapshot.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn wait_loading<R: LoadingRig>(
    rig: &mut R,
    opener: &mut LoadingOpener,
    committed: bool,
) -> Result<bool, LiveError<R::ScreenError>> {
    loop {
        if opener.latch == PrepState::Pending {
            let deadline = std::time::Instant::now() + PREP_SLICE;
            if let PrepPoll::Done(answer) = rig
                .poll_preparation(&mut opener.handle, deadline)
                .map_err(LiveError::Screen)?
            {
                opener.latch = PrepState::of(answer);
            }
        } else {
            rig.wait();
        }
        // The exits in order, those that end the run first: a thread's
        // failure, a refusal, a close, and after the commit a lost screen
        // (design.md "Rendering and Skins": nothing would draw the play);
        // then the player's quit and a cancelled preparation.
        threads_over(rig)?;
        if let PrepState::Refused(why) = &opener.latch {
            return Err(LiveError::Preparation(why.clone()));
        }
        if rig.quitting() {
            return Ok(false);
        }
        // One look at the screen per pass, used by both exits below: a
        // screen lost after it is seen at the next pass.
        let lost = rig.screen_failed();
        if committed && lost {
            return Err(LiveError::ScreenLost);
        }
        if quit_look(rig) || opener.latch == PrepState::Cancelled {
            return Ok(false);
        }
        // A screen built again keeps the loading lifetime: its first
        // frame, if not presented yet, still starts the minimum. After the
        // commit nothing is built again: the run ends instead.
        if lost {
            if committed {
                return Err(LiveError::ScreenLost);
            }
            rig.prepare_screen().map_err(LiveError::Screen)?;
        }
        if opener.ack.is_none()
            && let Some((lifetime, at)) = rig.loading_ack()
            && lifetime == opener.lifetime
        {
            opener.ack = Some(at);
        }
        let now = rig.now();
        let tuning = rig.tuning_snapshot().map_err(LiveError::Input)?;
        if opener.minimum_passed(rig, now) && tuning.quiet_at(now) {
            return Ok(true);
        }
    }
}

impl<R: LoadingRig> LoadingGate<R> for LoadingOpener {
    fn open(
        &mut self,
        loaded: &mut Loaded,
        rig: &mut R,
        lead_in: TimeUs,
        generation: u32,
    ) -> Result<Option<Opening<R::Reader>>, LiveError<R::ScreenError>> {
        open_loading(self, loaded, rig, lead_in, generation)
    }
}

/// Opens candidate `generation` out of the loading state (design.md "Play
/// Lifecycle", the start gate): a deferral reopens tuning acceptance and
/// resumes the loading presentation ([`Rig::defer`]); a held START or
/// SELECT is waited out to its release, a focus deferral until focus is
/// back, both released and the tuning quiet for [`QUIET`] from the
/// return. Meanwhile the quit key, a close and the threads' and the
/// screen's failures are served. Acceptance then closes again on a fresh
/// snapshot, applied to the starting settings, loading ends again and the
/// same generation is asked again, as often as it defers. `None` when the
/// player left before it opened.
///
/// # Errors
///
/// The threads' failures, and a screen that does not end the loading
/// state, which end the run as before any opening.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn open_loading<R: LoadingRig>(
    opener: &mut LoadingOpener,
    loaded: &mut Loaded,
    rig: &mut R,
    lead_in: TimeUs,
    generation: u32,
) -> Result<Option<Opening<R::Reader>>, LiveError<R::ScreenError>> {
    loop {
        let opened = if rig.gated() {
            rig.open_candidate(lead_in, generation)
        } else {
            rig.open(lead_in).map(Some)
        };
        if let Some(opening) = opened.map_err(LiveError::Input)? {
            // Recording opened: acceptance reopened with it.
            opener.closed = false;
            return Ok(Some(opening));
        }
        let mut for_focus = rig.focus_deferred();
        rig.defer();
        rig.reopen_acceptance().map_err(LiveError::Input)?;
        opener.closed = false;
        let snapshot = loop {
            rig.wait();
            preflight(rig)?;
            if quit_look(rig) {
                return Ok(None);
            }
            for_focus |= rig.focus_deferred();
            if rig.holding() {
                continue;
            }
            if for_focus {
                let tuning = rig.tuning_snapshot().map_err(LiveError::Input)?;
                if !tuning.quiet_at(rig.now()) {
                    continue;
                }
            }
            let snapshot = rig.close_acceptance(generation).map_err(LiveError::Input)?;
            opener.closed = true;
            // Deferred for focus, the closed copy is decided quiet itself:
            // an event since the look sends it back to waiting.
            if !for_focus || snapshot.quiet_at(rig.now()) {
                break snapshot;
            }
            rig.reopen_acceptance().map_err(LiveError::Input)?;
            opener.closed = false;
        };
        rig.apply_snapshot(&snapshot, loaded.settings_mut());
        rig.end_loading().map_err(LiveError::Screen)?;
        // The last look before recording opens again.
        preflight(rig)?;
        if quit_look(rig) {
            return Ok(None);
        }
    }
}
