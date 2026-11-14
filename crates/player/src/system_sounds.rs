//! The system sounds' owner on the game thread (design.md "Audio", system
//! sounds): the `system-sounds` worker that picks and decodes the sets, the
//! ledger of what the picks hold, the requests that start and stop the
//! sounds, and tone-only recovery.
//!
//! **The worker.** One thread for the run owns the recovery state
//! (`sound-sets.toml` and its lock, design.md T18) and runs one pick at a
//! time on request, answering each with a [`PickReport`].
//!
//! **Admission.** A pick is admitted only at startup ([`SystemSounds::new`])
//! and by an entry's re-roll ([`SystemSounds::repick`]), and only while no
//! pick is decoding, no install is unacknowledged and no generation is
//! retired; it reserves [`PICK_BYTES`] before it starts. So at most one
//! generation is retired and the current, the retired one and the
//! reservation stay within the system sounds' total ("Memory Budget").
//! Nothing else admits: [`poll`](SystemSounds::poll) and the settlement
//! before recording only receive.
//!
//! **The mapping.** The sounds of the newest generation pushed, per kind,
//! are what the next pick keeps for a kind it finds no candidate for.
//!
//! **The latch.** Once recovery is unavailable (the lock, the state file,
//! a commit or a clear failed, the worker is gone, or the request sequence
//! is exhausted) every set sound stops for the rest of the run through the
//! empty generation, and the decision sound is the tone. The latch runs in
//! play too (a refused draw of [`start`](SystemSounds::start) or
//! [`stop`](SystemSounds::stop)), so it frees and allocates nothing: it
//! moves the mapping into a slot made with the owner, and the next drain
//! outside play drops it and names the latch, before that call releases
//! any return's charge.
//!
//! Lines this owner names for the player (refused files, a quarantined or
//! skipped source, the latch) go to the `say` hook its caller gives, and
//! lines only for the log (skipped re-rolls) to the `log` hook; this file
//! never names the log facade, since [`SystemSounds::start`],
//! [`SystemSounds::stop`] and [`SystemSounds::latch`] are in-play code.

use std::collections::HashSet;
use std::fmt;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audio::{BankLimits, GenId, PickBudget, SystemGeneration, SystemKind};
use game::sound_sets::{
    DecodeFile, KeptSounds, OneFile, PICK_BYTES, PickDecode, PickReport, Recovery, RecoveryFs,
    ScanCap, ScanLimits, SetPick, SetRole, SourceIdentity, Unavailable, choose, decode_pick,
    random_index, scan,
};
use game::{
    AudioControl, AudioWaitError, DataRoot, SWITCH_TIMEOUT, SoundSettings, SystemStartError,
    TERMINAL,
};

use crate::imports::STOP_WAIT;

/// How long an entry to selection waits for the previous pick to settle
/// (its report, its install's acknowledgement and the return of the
/// generation it retired) before it keeps the current sets for that visit.
/// Provisional.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the selection entry's re-roll is wired later")
)]
pub const ENTRY_SETTLE_WAIT: Duration = Duration::from_secs(2);

/// One slice of the owner's waits.
const SLICE: Duration = Duration::from_millis(1);

/// Why the run's sets are off: every set sound stops and the decision
/// sound is the tone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The recovery state is unavailable (its lock, its file, a commit or
    /// a clear).
    Recovery(Unavailable),
    /// The worker could not start, or ended.
    WorkerGone,
    /// The request sequence is exhausted.
    SequenceExhausted,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recovery(u) => u.fmt(f),
            Self::WorkerGone => f.write_str("the system sounds' worker is not running"),
            Self::SequenceExhausted => f.write_str("the system sounds' request numbers ran out"),
        }
    }
}

/// How the settlement before recording ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// Every pick reported and what was pushed is acknowledged.
    Ready,
    /// The quit key, a close or a thread failure came first.
    Cancelled,
}

/// The clock of the owner's waits: the system's, or a test's.
pub trait Clock: Send + Sync {
    /// Now.
    fn now(&self) -> Instant;
    /// Waits about `d`.
    fn pause(&self, d: Duration);
}

/// The system's clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn pause(&self, d: Duration) {
        thread::sleep(d);
    }
}

/// Starts the worker's thread: the system's, or a test's seam.
pub type Spawn = fn(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>;

/// Starts `body` on a thread named `system-sounds`.
pub fn spawn_worker(body: Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("system-sounds".to_owned())
        .spawn(body)
}

/// What the worker needs for the run.
pub struct Setup {
    /// The data root: the recovery state, `defaultsound\` and the default
    /// set roots.
    pub root: DataRoot,
    /// `player.toml`'s `[sounds]`.
    pub sounds: SoundSettings,
    /// The output's rate, which the sounds are decoded at.
    pub rate: u32,
    /// The per-file caps.
    pub limits: BankLimits,
    /// The scan's caps.
    pub scan: ScanLimits,
    /// The recovery state's file system.
    pub fs: Box<dyn RecoveryFs>,
    /// How one file decodes.
    pub decoder: Box<dyn DecodeFile + Send>,
    /// An index below its argument: which set a pick takes.
    pub picker: Box<dyn FnMut(usize) -> usize + Send>,
}

impl Setup {
    /// The run's setup: the disk, the decoders and a random pick.
    pub fn new(root: DataRoot, sounds: SoundSettings, rate: u32) -> Self {
        Self {
            root,
            sounds,
            rate,
            limits: BankLimits::default(),
            scan: ScanLimits::default(),
            fs: crate::sound_sets_disk::recovery_fs(),
            decoder: Box::new(OneFile),
            picker: Box::new(random_index),
        }
    }
}

/// How the owner waits, starts its worker and names what it names.
pub struct Hooks {
    /// Takes each line the owner names for the player, once each.
    pub say: Box<dyn FnMut(&str) + Send>,
    /// Takes each line only for the log: an entry's skipped re-roll, and
    /// the run's count of them.
    pub log: Box<dyn FnMut(&str) + Send>,
    /// The waits' clock.
    pub clock: Arc<dyn Clock>,
    /// Starts the worker.
    pub spawn: Spawn,
}

/// Tests: what sets up an owner the rig makes.
#[cfg(test)]
type Arming = Box<dyn FnOnce(&mut Setup, &mut Hooks)>;

#[cfg(test)]
thread_local! {
    /// Tests: what arms the next owner made on this thread, through the
    /// rig ([`arm`]).
    static ARMED: std::cell::RefCell<Option<Arming>> = const { std::cell::RefCell::new(None) };
}

/// Tests: the next owner the rig makes on this thread is set up through
/// `f` (its sets, decoder, recovery state and clock).
#[cfg(test)]
pub fn arm(f: impl FnOnce(&mut Setup, &mut Hooks) + 'static) {
    ARMED.with_borrow_mut(|a| *a = Some(Box::new(f)));
}

/// Tests: applies what [`arm`] set, once.
#[cfg(test)]
pub fn take_armed(setup: &mut Setup, hooks: &mut Hooks) {
    if let Some(f) = ARMED.with_borrow_mut(Option::take) {
        f(setup, hooks);
    }
}

/// What the game thread asks the worker.
struct Ask {
    id: GenId,
    kept: KeptSounds,
    skipped: Vec<SourceIdentity>,
    cancel: Arc<AtomicBool>,
}

/// What the worker answers.
enum Reply {
    /// The recovery state is open; a marker found was quarantined.
    Opened(Option<SourceIdentity>),
    /// The recovery state is unavailable: the worker ends.
    Unavailable(Unavailable),
    /// A pick's report, with the scans' caps reached.
    Picked {
        id: GenId,
        report: Box<PickReport>,
        caps: Vec<ScanCap>,
    },
}

/// The worker's handle.
struct Worker {
    asks: Option<SyncSender<Ask>>,
    replies: Receiver<Reply>,
    handle: Option<JoinHandle<()>>,
}

/// The pick decoding now.
#[derive(Debug)]
struct InFlight {
    id: GenId,
    cancel: Arc<AtomicBool>,
    /// An entry gave up waiting for it: its report is dropped.
    superseded: bool,
}

/// What a test reads of the owner's order of work.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A pick admitted.
    Admitted(GenId),
    /// A generation pushed onto the install ring.
    Pushed(GenId),
    /// A report dropped unpushed.
    Dropped(GenId),
    /// The deferred-drop slot dropped.
    Drained,
    /// A returned generation's charge released.
    Released(GenId),
}

/// The system sounds' owner on the game thread.
pub struct SystemSounds {
    worker: Option<Worker>,
    /// The worker answers nothing more.
    worker_ended: bool,
    hooks: Hooks,
    /// Every non-empty generation handed to the audio thread and not yet
    /// returned, with its bytes.
    alive: Vec<(GenId, u64)>,
    in_flight: Option<InFlight>,
    /// The newest generation pushed and not yet acknowledged.
    unacked: Option<GenId>,
    /// The generation the audio thread acknowledged last.
    installed: Option<GenId>,
    /// The newest generation pushed, the empty one included.
    newest: GenId,
    /// The sounds of the newest generation pushed, per kind.
    mapping: KeptSounds,
    /// The mapping a latch invalidated, dropped by the next drain.
    deferred: Option<KeptSounds>,
    latched: Option<Reason>,
    latch_named: bool,
    /// An entry's wait timed out and I3 has not held since.
    stalled: bool,
    /// The first entry after startup has come.
    entered: bool,
    /// A run-ending signal ended an entry's wait.
    entry_ended: bool,
    closed: bool,
    /// Sources skipped for the run: a decode of theirs panicked.
    skipped: Vec<SourceIdentity>,
    skipped_rerolls: u64,
    /// Starts refused while latched.
    refused: u64,
    named: HashSet<String>,
    #[cfg(test)]
    events: Vec<Event>,
    #[cfg(test)]
    max_retired: usize,
    /// Empty installs the latch pushed.
    #[cfg(test)]
    latch_installs: u32,
    /// Calls of [`poll`](Self::poll).
    #[cfg(test)]
    polls: u32,
}

/// The worker's loop: open the recovery state, then one pick per ask.
fn work(mut setup: Setup, asks: Receiver<Ask>, replies: Sender<Reply>) {
    let opened = match Recovery::open(&setup.root, setup.fs) {
        Ok(opened) => opened,
        Err(u) => {
            let _ = replies.send(Reply::Unavailable(u));
            return;
        }
    };
    let mut recovery = opened.recovery;
    if replies.send(Reply::Opened(opened.quarantined)).is_err() {
        return;
    }
    let defaults = setup.root.default_sounds();
    while let Ok(Ask {
        id,
        kept,
        skipped,
        cancel,
    }) = asks.recv()
    {
        let mut caps = Vec::new();
        let mut set = |role: SetRole, recovery: &mut Recovery| -> Option<PathBuf> {
            let root = setup.sounds.root(role, &setup.root)?;
            let scanned = scan(&root, role, &setup.scan);
            caps.extend(scanned.caps);
            choose(&scanned.sets, recovery, &skipped, &mut *setup.picker)
        };
        let sound_set = set(SetRole::Sound, &mut recovery);
        let bgm_set = set(SetRole::Bgm, &mut recovery);
        let pick = SetPick {
            sound_set,
            bgm_set,
            defaults: defaults.clone(),
            id,
            rate: setup.rate,
            limits: setup.limits,
            skipped,
        };
        let budget = PickBudget::new(PICK_BYTES);
        let cancelled = move || cancel.load(Ordering::Acquire);
        let report = decode_pick(
            &pick,
            &kept,
            &budget,
            &mut recovery,
            &cancelled,
            &*setup.decoder,
        );
        // The kept sounds' clones go before the report, so the game thread
        // holds every share of them once it has the report.
        drop((kept, pick, budget));
        let reply = Reply::Picked {
            id,
            report: Box::new(report),
            caps,
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

impl SystemSounds {
    /// Starts the worker and admits the startup pick, whose generation it
    /// pushes once ready. A worker that cannot start latches the run, with
    /// nothing reserved.
    pub fn new(control: &mut AudioControl, setup: Setup, hooks: Hooks) -> Self {
        let (ask_tx, ask_rx) = mpsc::sync_channel::<Ask>(1);
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();
        let spawned = (hooks.spawn)(Box::new(move || work(setup, ask_rx, reply_tx)));
        let mut sounds = Self {
            worker: None,
            worker_ended: false,
            hooks,
            alive: Vec::with_capacity(audio::SYSTEM_RETURNS + 1),
            in_flight: None,
            unacked: None,
            installed: None,
            newest: GenId(0),
            mapping: KeptSounds::default(),
            deferred: None,
            latched: None,
            latch_named: false,
            stalled: false,
            entered: false,
            entry_ended: false,
            closed: false,
            skipped: Vec::new(),
            skipped_rerolls: 0,
            refused: 0,
            named: HashSet::new(),
            #[cfg(test)]
            events: Vec::new(),
            #[cfg(test)]
            max_retired: 0,
            #[cfg(test)]
            latch_installs: 0,
            #[cfg(test)]
            polls: 0,
        };
        match spawned {
            Ok(handle) => {
                sounds.worker = Some(Worker {
                    asks: Some(ask_tx),
                    replies: reply_rx,
                    handle: Some(handle),
                });
                sounds.admit(control);
            }
            Err(e) => {
                sounds.worker_ended = true;
                sounds.say_once(format!("system sounds: the worker cannot start: {e}"));
                sounds.latch(control, Reason::WorkerGone);
            }
        }
        sounds.drain_deferred();
        sounds
    }

    /// Starts `kind`, looped and bound to generation `looped` when given
    /// (it plays only from that one). Once latched every set kind's start
    /// is refused and counted, and the decision sound is the tone. A start
    /// whose number is refused latches the run; a decision sound then
    /// sounds the tone. It allocates and frees nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the triggers come with their screens")
    )]
    pub fn start(&mut self, control: &mut AudioControl, kind: SystemKind, looped: Option<GenId>) {
        if self.latched.is_some() {
            if kind == SystemKind::Decide {
                control.play_tone();
            } else {
                self.refused += 1;
            }
            return;
        }
        if let Err(SystemStartError::Exhausted) =
            control.system_start(kind, looped.is_some(), looped)
        {
            self.latch(control, Reason::SequenceExhausted);
            if kind == SystemKind::Decide {
                control.play_tone();
            }
        }
    }

    /// Stops every sounding `kind`, in every generation. A stop is never
    /// refused; one whose number is refused latches the run. It allocates
    /// and frees nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    #[cfg_attr(
        not(all(test, feature = "dev-tools")),
        expect(dead_code, reason = "the triggers come with their screens")
    )]
    pub fn stop(&mut self, control: &mut AudioControl, kind: SystemKind) {
        if control.system_stop(kind).is_none() {
            self.latch(control, Reason::SequenceExhausted);
        }
    }

    /// Latches the run for `reason`, once: every set kind stops under one
    /// number and the empty generation is pushed, so no set sound plays
    /// again and the decision sound is the tone; the mapping moves into the
    /// deferred-drop slot, so a later pick keeps nothing. It allocates and
    /// frees nothing: the slot is dropped, and the latch named, by the next
    /// drain outside play.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn latch(&mut self, control: &mut AudioControl, reason: Reason) {
        if self.latched.is_some() {
            return;
        }
        self.latched = Some(reason);
        let id = GenId(control.system_stop_all().unwrap_or(TERMINAL));
        if control.install_system(SystemGeneration::empty(id)).is_ok() {
            self.unacked = Some(id);
            self.newest = id;
            #[cfg(test)]
            {
                self.latch_installs += 1;
            }
        }
        self.deferred = Some(mem::take(&mut self.mapping));
    }

    /// Receives what the worker and the audio thread have given back:
    /// a report, pushing a ready generation at once; the acknowledgement;
    /// the returned generations, dropped here and uncharged. Never admits.
    /// Called outside play only.
    pub fn poll(&mut self, control: &mut AudioControl) {
        #[cfg(test)]
        {
            self.polls += 1;
        }
        self.service(control);
        self.drain_deferred();
    }

    /// The entry to selection's re-roll: stops every set kind of the last
    /// visit, then, past the first entry and unlatched, waits in slices up
    /// to [`ENTRY_SETTLE_WAIT`] for the previous pick to settle and admits
    /// a fresh pick. Returns the generation the visit's loop binds to: the
    /// pick admitted, or, when the wait times out (counted and said once
    /// per entry) or `ends` ends it (the entry's end, taken by
    /// [`take_entry_end`](Self::take_entry_end)), the newest pushed. An
    /// entry while an earlier one's timeout is still outstanding skips at
    /// once.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the selection entry's re-roll is wired later")
    )]
    pub fn repick(&mut self, control: &mut AudioControl, mut ends: impl FnMut() -> bool) -> GenId {
        if control.system_stop_all().is_none() {
            self.latch(control, Reason::SequenceExhausted);
        }
        let id = self.entry(control, &mut ends);
        self.drain_deferred();
        id
    }

    fn entry(&mut self, control: &mut AudioControl, ends: &mut dyn FnMut() -> bool) -> GenId {
        self.service(control);
        if self.latched.is_some() {
            return self.newest;
        }
        if !mem::replace(&mut self.entered, true) {
            return self.in_flight.as_ref().map_or(self.newest, |f| f.id);
        }
        if self.stalled {
            self.skip();
            return self.newest;
        }
        let deadline = self.hooks.clock.now() + ENTRY_SETTLE_WAIT;
        loop {
            self.service(control);
            if self.latched.is_some() {
                return self.newest;
            }
            if ends() {
                self.entry_ended = true;
                return self.newest;
            }
            if self.settled() {
                return self.admit(control).unwrap_or(self.newest);
            }
            if self.hooks.clock.now() >= deadline {
                if let Some(f) = self.in_flight.as_mut() {
                    f.cancel.store(true, Ordering::Release);
                    f.superseded = true;
                }
                self.stalled = true;
                self.skip();
                return self.newest;
            }
            self.hooks.clock.pause(SLICE);
        }
    }

    /// An entry's skipped re-roll: counted and said.
    fn skip(&mut self) {
        let what = if self.in_flight.is_some() {
            "the pick's report"
        } else if self.unacked.is_some() {
            "the install's acknowledgement"
        } else {
            "the retired generation's return"
        };
        self.skipped_rerolls += 1;
        (self.hooks.log)(&format!(
            "system sounds: the sets were not picked again at this entry: {what} did not come within {} s",
            ENTRY_SETTLE_WAIT.as_secs()
        ));
    }

    /// Whether an entry's wait ended the run's way: taken once.
    pub const fn take_entry_end(&mut self) -> bool {
        mem::replace(&mut self.entry_ended, false)
    }

    /// Before recording opens: waits, in slices and with no time bound, for
    /// every outstanding pick's report (one in flight is pushed, one an
    /// entry gave up on is dropped), then for the acknowledgement of what
    /// was pushed within [`SWITCH_TIMEOUT`]. `quit` (the quit key) or
    /// `ends` (a close or a thread failure) ends it within a slice, the
    /// pick left as it is. Never admits.
    ///
    /// # Errors
    ///
    /// [`AudioWaitError::Timeout`] when the acknowledgement does not come
    /// within [`SWITCH_TIMEOUT`], [`AudioWaitError::Gone`] when the audio
    /// thread has ended.
    pub fn settle_before_recording(
        &mut self,
        control: &mut AudioControl,
        mut quit: impl FnMut() -> bool,
        mut ends: impl FnMut() -> bool,
    ) -> Result<Settled, AudioWaitError> {
        let settled = self.settle(control, &mut quit, &mut ends);
        self.drain_deferred();
        settled
    }

    fn settle(
        &mut self,
        control: &mut AudioControl,
        quit: &mut dyn FnMut() -> bool,
        ends: &mut dyn FnMut() -> bool,
    ) -> Result<Settled, AudioWaitError> {
        let mut acknowledged_by = None;
        loop {
            self.service(control);
            if quit() || ends() {
                return Ok(Settled::Cancelled);
            }
            if self.in_flight.is_none() {
                if self.unacked.is_none() {
                    return Ok(Settled::Ready);
                }
                if control.gone() {
                    return Err(AudioWaitError::Gone);
                }
                let now = self.hooks.clock.now();
                if now >= *acknowledged_by.get_or_insert(now + SWITCH_TIMEOUT) {
                    return Err(AudioWaitError::Timeout);
                }
            }
            self.hooks.clock.pause(SLICE);
        }
    }

    /// Ends the worker (its pick cancelled, waited for within
    /// [`STOP_WAIT`], then left to end with the process), drops what the
    /// game thread holds and takes the returns, the deferred-drop slot
    /// drained before any return's charge is released. Idempotent.
    pub fn close(&mut self, control: &mut AudioControl) {
        if self.closed {
            return;
        }
        self.drain_deferred();
        self.end_worker();
        self.take_returns(control);
        self.drain_deferred();
        if self.skipped_rerolls > 0 {
            let n = self.skipped_rerolls;
            (self.hooks.log)(&format!(
                "system sounds: {n} selection entries kept the sets they had"
            ));
        }
        self.closed = true;
    }

    /// Cancels the pick, lets the worker's loop end, receives what it
    /// answers until its thread ends or [`STOP_WAIT`] passes, and releases
    /// the reservation.
    fn end_worker(&mut self) {
        self.closed = true;
        if let Some(f) = &self.in_flight {
            f.cancel.store(true, Ordering::Release);
        }
        if let Some(w) = self.worker.as_mut() {
            w.asks = None;
        }
        let until = self.hooks.clock.now() + STOP_WAIT;
        while !self.worker_ended {
            self.receive_worker();
            if self.worker_ended {
                break;
            }
            if self.hooks.clock.now() >= until {
                self.say_once(
                    "system sounds: the worker was left to end with the process".to_owned(),
                );
                if let Some(w) = self.worker.as_mut() {
                    w.handle = None;
                }
                self.worker_ended = true;
                break;
            }
            self.hooks.clock.pause(SLICE);
        }
        if let Some(h) = self.worker.as_mut().and_then(|w| w.handle.take()) {
            let _ = h.join();
        }
        self.in_flight = None;
    }

    /// The receivers' work, in the order the drain needs: the slot after
    /// the replies (which can latch) and before the returns, so a latch's
    /// mapping is dropped before any return's charge is released.
    fn service(&mut self, control: &mut AudioControl) {
        self.receive(control);
        self.drain_deferred();
        self.acknowledge(control);
        self.take_returns(control);
        #[cfg(test)]
        {
            self.max_retired = self.max_retired.max(self.retired_count());
        }
    }

    /// Drops the deferred-drop slot and names a new latch, once.
    fn drain_deferred(&mut self) {
        if let Some(kept) = self.deferred.take() {
            drop(kept);
            #[cfg(test)]
            self.events.push(Event::Drained);
        }
        if !self.latch_named
            && let Some(reason) = &self.latched
        {
            self.latch_named = true;
            let line =
                format!("system sounds: {reason}: only the decision tone sounds for this run");
            (self.hooks.say)(&line);
        }
    }

    fn receive(&mut self, control: &mut AudioControl) {
        while let Some(reply) = self.next_reply() {
            self.handle(control, reply);
        }
        if self.worker_ended && self.latched.is_none() {
            self.in_flight = None;
            self.latch(control, Reason::WorkerGone);
        }
    }

    /// Receives without a control: what `close` takes, every report
    /// dropped.
    fn receive_worker(&mut self) {
        while let Some(reply) = self.next_reply() {
            if let Reply::Picked { id, report, .. } = reply {
                self.dropped(id, *report);
            }
        }
    }

    fn next_reply(&mut self) -> Option<Reply> {
        if self.worker_ended {
            return None;
        }
        let w = self.worker.as_mut()?;
        match w.replies.try_recv() {
            Ok(reply) => Some(reply),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.worker_ended = true;
                if let Some(h) = w.handle.take() {
                    let _ = h.join();
                }
                None
            }
        }
    }

    fn handle(&mut self, control: &mut AudioControl, reply: Reply) {
        match reply {
            Reply::Opened(quarantined) => {
                if let Some(q) = quarantined {
                    self.say_once(format!(
                        "system sounds: {q} is skipped: a launch ended while it was decoding"
                    ));
                }
            }
            Reply::Unavailable(u) => {
                self.worker_ended = true;
                self.in_flight = None;
                self.latch(control, Reason::Recovery(u));
            }
            Reply::Picked { id, report, caps } => {
                for cap in caps {
                    self.say_once(format!("system sounds: {cap}"));
                }
                self.picked(control, id, *report);
            }
        }
    }

    fn picked(&mut self, control: &mut AudioControl, id: GenId, mut report: PickReport) {
        for r in mem::take(&mut report.refused) {
            self.say_once(r.to_string());
        }
        for s in mem::take(&mut report.panicked) {
            self.say_once(format!(
                "system sounds: {s} is skipped for this run: its decoding failed"
            ));
            if !self.skipped.contains(&s) {
                self.skipped.push(s);
            }
        }
        let flight = self.in_flight.take_if(|f| f.id == id);
        let superseded = flight.as_ref().is_none_or(|f| f.superseded);
        let recovery = mem::replace(&mut report.recovery, Ok(()));
        if let Err(u) = recovery {
            self.dropped(id, report);
            self.latch(control, Reason::Recovery(u));
            return;
        }
        if superseded || self.latched.is_some() || self.closed {
            self.dropped(id, report);
            return;
        }
        let PickDecode::Ready(generation) = report.decode else {
            return;
        };
        self.push(control, generation, report.kept);
    }

    /// A report not pushed: its generation dropped here, uncharged.
    #[cfg_attr(
        not(test),
        expect(unused_variables, reason = "only the test event names the generation")
    )]
    fn dropped(&mut self, id: GenId, report: PickReport) {
        drop(report);
        #[cfg(test)]
        self.events.push(Event::Dropped(id));
    }

    fn push(&mut self, control: &mut AudioControl, generation: SystemGeneration, kept: KeptSounds) {
        let id = generation.id();
        let bytes = generation.bytes();
        match control.install_system(generation) {
            Ok(_) => {
                self.alive.push((id, bytes));
                self.unacked = Some(id);
                self.newest = id;
                // The previous mapping is dropped here, outside play.
                self.mapping = kept;
                #[cfg(test)]
                self.events.push(Event::Pushed(id));
            }
            Err(game::SystemInstallError::Exhausted(g)) => {
                drop(g);
                self.latch(control, Reason::SequenceExhausted);
            }
            Err(e) => {
                self.say_once(format!("system sounds: a pick was not installed: {e}"));
            }
        }
    }

    fn acknowledge(&mut self, control: &AudioControl) {
        let Some(installed) = control.system_installed() else {
            return;
        };
        self.installed = Some(installed);
        if self.unacked.is_some_and(|u| installed >= u) {
            self.unacked = None;
        }
    }

    /// Takes every returned generation and drops it here, releasing its
    /// charge.
    fn take_returns(&mut self, control: &mut AudioControl) {
        while let Some(generation) = control.take_system_return() {
            let id = generation.id();
            drop(generation);
            self.alive.retain(|&(a, _)| a != id);
            #[cfg(test)]
            self.events.push(Event::Released(id));
        }
        if self.stalled && self.settled() {
            self.stalled = false;
        }
    }

    /// Admits a pick: reserves its bytes and asks the worker.
    fn admit(&mut self, control: &mut AudioControl) -> Option<GenId> {
        if self.latched.is_some() || self.worker_ended || self.closed {
            return None;
        }
        let Some(seq) = control.system_seq() else {
            self.latch(control, Reason::SequenceExhausted);
            return None;
        };
        let id = GenId(seq);
        let cancel = Arc::new(AtomicBool::new(false));
        let ask = Ask {
            id,
            kept: self.mapping.clone(),
            skipped: self.skipped.clone(),
            cancel: Arc::clone(&cancel),
        };
        let sent = self
            .worker
            .as_ref()
            .and_then(|w| w.asks.as_ref())
            .is_some_and(|tx| tx.try_send(ask).is_ok());
        if !sent {
            // The worker has ended: what it answered first (a recovery state
            // it could not open) names the latch.
            while let Some(reply) = self.next_reply() {
                self.handle(control, reply);
            }
            self.worker_ended = true;
            self.latch(control, Reason::WorkerGone);
            return None;
        }
        self.in_flight = Some(InFlight {
            id,
            cancel,
            superseded: false,
        });
        #[cfg(test)]
        self.events.push(Event::Admitted(id));
        Some(id)
    }

    /// I3: no pick decoding, no install unacknowledged, no generation
    /// retired.
    fn settled(&self) -> bool {
        self.in_flight.is_none() && self.unacked.is_none() && self.retired_count() == 0
    }

    /// Generations handed over that are neither the installed one nor
    /// awaiting their acknowledgement.
    fn retired_count(&self) -> usize {
        self.alive
            .iter()
            .filter(|&&(id, _)| Some(id) != self.installed && Some(id) != self.unacked)
            .count()
    }

    fn say_once(&mut self, line: String) {
        if self.named.insert(line.clone()) {
            (self.hooks.say)(&line);
        }
    }

    /// The bytes charged: every generation handed over and not returned,
    /// and the reservation of the pick decoding.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the tests read the charge")
    )]
    pub fn charged(&self) -> u64 {
        self.alive.iter().map(|&(_, b)| b).sum::<u64>()
            + self.in_flight.as_ref().map_or(0, |_| PICK_BYTES)
    }
}

#[cfg(test)]
impl SystemSounds {
    /// Whether a pick is decoding, and whether it was asked to stop.
    pub(crate) fn pick_running(&self) -> Option<bool> {
        self.in_flight
            .as_ref()
            .map(|f| f.cancel.load(Ordering::Acquire))
    }

    /// The generations pushed, in order.
    pub(crate) fn pushed(&self) -> Vec<GenId> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Pushed(id) => Some(*id),
                _ => None,
            })
            .collect()
    }
}

impl Drop for SystemSounds {
    /// Ends the worker as [`close`](SystemSounds::close) does; what the
    /// audio thread holds goes with its shared state.
    fn drop(&mut self) {
        if !self.closed {
            self.end_worker();
        }
    }
}

#[cfg(all(test, feature = "dev-tools"))]
mod rig_tests;
#[cfg(test)]
pub(crate) mod tests;
