//! The settings keys' values while the game runs (design.md "Input"): the
//! window thread steps them on F3 to F8, the adjustments' driver reads the
//! display offset at each drain, and the game thread forwards the volume to
//! the audio thread, takes the input offset into the next play and writes
//! the files when a play ends. The values are read lock-free; nothing here
//! allocates.
//!
//! Tuning acceptance (design.md "Play Lifecycle", tuning while loading) is
//! kept under the step lock beside the values: the window side's revision,
//! its last activity, the settings keys that are down with how each came
//! to be ([`HoldClass`]), an overflow of that set, whether the window has
//! focus with its focus losses, and whether acceptance is open or closed
//! until a candidate generation's recording opens. Every change of them is
//! written, under the lock, into the window side's section of the activity
//! record ([`input::ActivityRecord`]); the input thread's adjustment tap
//! writes the tuning controls it sees (START, SELECT, F1, F2) into the
//! input side's. A copy of the values with the record read as one
//! snapshot ([`LiveTuning::snapshot`]) is what the loading state and the
//! opening read.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bms_model::TimeUs;
use game::{
    AcceptanceRevision, Adjust, Chain, DataRoot, Settings, Tuning, TuningSnapshot, save_settings,
};
use input::{ActivityRecord, SectionValue};

/// The settings keys the window side holds at most: one entry per bound
/// settings key.
pub const MAX_HELD: usize = Adjust::ALL.len();

/// How a settings key that is down came to be: what its repeats and its
/// release do, and whether it holds the loading state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldClass {
    /// Pressed at or after the loading state began, focused, while
    /// acceptance was open: its repeats step, its release counts.
    Fresh,
    /// Down when the loading state began: it holds the loading state until
    /// released, steps nothing, and its release counts.
    PreLoading,
    /// Down when focus was lost: its release may never come, so it holds
    /// nothing, and its repeats and release do nothing.
    FocusDisarmed,
    /// Pressed while acceptance was closed: it holds nothing, and its
    /// repeats and release do nothing.
    GapDiscarded,
}

impl HoldClass {
    /// Whether a key of this class holds the loading state and the
    /// opening.
    const fn blocks(self) -> bool {
        matches!(self, Self::Fresh | Self::PreLoading)
    }
}

/// What a settings key did, as the window saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    /// It went down.
    Press,
    /// The keyboard repeated it while down.
    Repeat,
    /// It went up.
    Release,
}

/// Whether tuning acceptance takes presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acceptance {
    /// Presses act.
    Open,
    /// Presses act on nothing until candidate generation `reopen_at`'s
    /// recording opens, or acceptance is reopened.
    Closed {
        /// The generation whose opening reopens it.
        reopen_at: u32,
    },
}

/// The window side's acceptance state, under the step lock.
#[derive(Debug)]
struct Window {
    /// Moves with every accepted press, accepted repeat, release of a key
    /// that holds, value change, overflow and focus change.
    revision: u64,
    /// The latest activity's time stamp, the largest seen.
    last: Option<TimeUs>,
    /// The settings keys that are down, one entry each.
    held: [Option<(Adjust, HoldClass)>; MAX_HELD],
    /// How many entries `held` takes: the bound settings keys.
    capacity: usize,
    /// A press did not fit `held`: it holds until focus loss clears it.
    overflow: bool,
    acceptance: Acceptance,
    /// When the current loading state began: a press before it is
    /// [`HoldClass::PreLoading`].
    epoch_start: TimeUs,
    focused: bool,
    /// Focus losses so far: the generation an opening accepts.
    focus_losses: u64,
    /// Focus was lost since acceptance last reopened.
    lost_since_reopen: bool,
}

impl Window {
    const fn new() -> Self {
        Self {
            revision: 0,
            last: None,
            held: [None; MAX_HELD],
            capacity: MAX_HELD,
            overflow: false,
            acceptance: Acceptance::Open,
            epoch_start: TimeUs(i64::MIN),
            focused: true,
            focus_losses: 0,
            lost_since_reopen: false,
        }
    }

    /// What the window side's section of the activity record holds.
    fn section(&self) -> SectionValue {
        SectionValue {
            revision: self.revision,
            last: self.last,
            held: self.blocking(),
            overflow: self.overflow,
            unfocused: !self.focused,
            focus_losses: self.focus_losses,
        }
    }

    /// The revision moves; the last activity moves to `at` if later.
    fn bump(&mut self, at: Option<TimeUs>) {
        self.revision = self.revision.wrapping_add(1);
        if let Some(at) = at {
            self.last = Some(self.last.map_or(at, |l| TimeUs(l.0.max(at.0))));
        }
    }

    /// A closed acceptance whose candidate's recording opened (`opened`,
    /// the input side's generation) is open: the input thread opened it
    /// without the step lock.
    fn normalize(&mut self, opened: u32) {
        if let Acceptance::Closed { reopen_at } = self.acceptance
            && opened >= reopen_at
        {
            self.acceptance = Acceptance::Open;
            self.lost_since_reopen = false;
        }
    }

    /// `a`'s entry, if it is down.
    fn entry(&mut self, a: Adjust) -> Option<&mut (Adjust, HoldClass)> {
        self.held
            .iter_mut()
            .take(self.capacity)
            .flatten()
            .find(|(k, _)| *k == a)
    }

    /// Puts `a` down as `class` in a free entry: `false` when none is free.
    fn insert(&mut self, a: Adjust, class: HoldClass) -> bool {
        match self
            .held
            .iter_mut()
            .take(self.capacity)
            .find(|e| e.is_none())
        {
            Some(free) => {
                *free = Some((a, class));
                true
            }
            None => false,
        }
    }

    /// Takes `a`'s entry out: its class, if it was down.
    fn remove(&mut self, a: Adjust) -> Option<HoldClass> {
        self.held
            .iter_mut()
            .find(|e| e.is_some_and(|(k, _)| k == a))
            .and_then(Option::take)
            .map(|(_, class)| class)
    }

    /// Whether a key down holds the loading state.
    fn blocking(&self) -> bool {
        self.held.iter().flatten().any(|(_, c)| c.blocks())
    }
}

/// The four values, shared, with tuning acceptance.
#[derive(Debug)]
pub struct LiveTuning {
    volume_percent: AtomicU32,
    /// The volume as loaded, exact (`f32` bits), which plays until a
    /// volume key moves it to a whole percent.
    loaded_volume: AtomicU32,
    /// Whether a volume key has moved the volume.
    volume_moved: AtomicBool,
    display_offset_us: AtomicI64,
    input_offset_us: AtomicI64,
    /// Counts the changes, so readers notice one.
    version: AtomicU64,
    /// Whether the end screen shows: only it takes the input offset keys.
    end_screen: AtomicBool,
    /// The step lock, with the window side's acceptance state: held by a
    /// step, by closing the end screen and by every acceptance change and
    /// copy, so an input offset step is either in the next play's values or
    /// refused, never lost between the two. The render thread reads the
    /// values without it.
    step: Mutex<Window>,
    /// The activity record: the window side's section written under the
    /// lock, the input side's by the input thread's tap without it, with
    /// the generation whose recording the input thread opened last.
    record: Arc<ActivityRecord>,
}

impl LiveTuning {
    /// The values `t`, with no end screen shown.
    pub fn new(t: Tuning) -> Self {
        Self {
            volume_percent: AtomicU32::new(t.volume_percent),
            loaded_volume: AtomicU32::new(t.volume().to_bits()),
            volume_moved: AtomicBool::new(false),
            display_offset_us: AtomicI64::new(t.display_offset_us),
            input_offset_us: AtomicI64::new(t.input_offset_us),
            version: AtomicU64::new(0),
            end_screen: AtomicBool::new(false),
            step: Mutex::new(Window::new()),
            record: Arc::default(),
        }
    }

    /// The values `t`, with the volume as loaded, `volume` (between whole
    /// percents, as `Tuning` keeps it), playing until a volume key moves it.
    pub fn with_loaded_volume(self, volume: f32) -> Self {
        self.loaded_volume
            .store(volume.to_bits(), Ordering::Release);
        self
    }

    /// The step lock.
    fn window(&self) -> MutexGuard<'_, Window> {
        self.step.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The activity record, for the input thread's tap to write its
    /// section of and to read the focus losses at an opening.
    pub const fn activity(&self) -> &Arc<ActivityRecord> {
        &self.record
    }

    /// The window's focus losses as last written: what an opening compares
    /// with the generation it accepted.
    pub fn focus_losses(&self) -> u64 {
        self.record.window.focus_losses()
    }

    /// Writes the window side's section from `w`, the step lock held.
    fn publish(&self, w: &Window) {
        self.record.window.publish(w.section());
    }

    /// The master volume to play: as loaded until a volume key moves it.
    pub fn volume(&self) -> f32 {
        if self.volume_moved.load(Ordering::Acquire) {
            self.get().volume()
        } else {
            f32::from_bits(self.loaded_volume.load(Ordering::Acquire))
        }
    }

    /// The current values. Only the window thread changes them, one at a
    /// time, so a read between two changes sees each value as it is.
    pub fn get(&self) -> Tuning {
        Tuning {
            volume_percent: self.volume_percent.load(Ordering::Acquire),
            display_offset_us: self.display_offset_us.load(Ordering::Acquire),
            input_offset_us: self.input_offset_us.load(Ordering::Acquire),
        }
    }

    /// The display offset now, µs: what the adjustments' driver reads at
    /// each drain.
    pub fn display_offset_us(&self) -> i64 {
        self.display_offset_us.load(Ordering::Acquire)
    }

    /// How many changes there have been.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// One step of a settings key, not from a key the window routes (a
    /// development script's): the input offset keys work on the end screen
    /// only, so a play never changes its own input offset (design.md
    /// "Offset Changes and the Watermark"). A change moves the revision.
    /// Returns whether a value changed.
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn adjust(&self, a: Adjust) -> bool {
        let mut w = self.window();
        if self.refused(a) {
            return false;
        }
        let changed = self.step_value(a);
        if changed {
            w.bump(None);
            self.publish(&w);
        }
        changed
    }

    /// Whether `a` is an input offset key while the end screen does not
    /// show: refused, it is neither a change nor activity (design.md "Play
    /// Lifecycle", tuning while loading).
    fn refused(&self, a: Adjust) -> bool {
        a.is_input_offset() && !self.end_screen.load(Ordering::Acquire)
    }

    /// Steps the value `a` owns, the step lock held: whether it changed.
    fn step_value(&self, a: Adjust) -> bool {
        let old = self.get();
        let new = old.adjust(a);
        if new == old {
            return false;
        }
        // Only the value the key owns is stored.
        match a {
            Adjust::VolumeDown | Adjust::VolumeUp => {
                self.volume_percent
                    .store(new.volume_percent, Ordering::Release);
                self.volume_moved.store(true, Ordering::Release);
            }
            Adjust::DisplayOffsetDown | Adjust::DisplayOffsetUp => {
                self.display_offset_us
                    .store(new.display_offset_us, Ordering::Release);
            }
            Adjust::InputOffsetDown | Adjust::InputOffsetUp => {
                self.input_offset_us
                    .store(new.input_offset_us, Ordering::Release);
            }
        }
        self.version.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Settings key `a` had `event` at `at`, from the window thread: by its
    /// class ([`HoldClass`]), a press steps the value and holds, a repeat of
    /// a fresh hold steps again, and a release of a key that holds counts;
    /// each moves the revision and the last activity. A press while
    /// acceptance is closed, or before the loading state began, steps
    /// nothing; one that does not fit is an overflow. A refused input offset
    /// key is nothing at all. Returns whether a value changed.
    pub fn key(&self, a: Adjust, event: KeyEvent, at: TimeUs) -> bool {
        let mut w = self.window();
        let changed = self.key_locked(&mut w, a, event, at);
        // Written before the lock is let go.
        self.publish(&w);
        changed
    }

    /// [`key`](Self::key), the step lock held.
    fn key_locked(&self, w: &mut Window, a: Adjust, event: KeyEvent, at: TimeUs) -> bool {
        w.normalize(self.record.opened());
        let refused = self.refused(a);
        match event {
            KeyEvent::Press => {
                if refused || !w.focused {
                    return false;
                }
                let class = if matches!(w.acceptance, Acceptance::Closed { .. }) {
                    HoldClass::GapDiscarded
                } else if at.0 < w.epoch_start.0 {
                    HoldClass::PreLoading
                } else {
                    HoldClass::Fresh
                };
                if let Some(entry) = w.entry(a) {
                    entry.1 = class;
                } else if !w.insert(a, class) {
                    if class.blocks() {
                        w.overflow = true;
                        w.bump(Some(at));
                    }
                    return false;
                }
                if class != HoldClass::Fresh {
                    return false;
                }
                w.bump(Some(at));
                self.step_value(a)
            }
            KeyEvent::Repeat => {
                let fresh = w.entry(a).is_some_and(|e| e.1 == HoldClass::Fresh);
                if refused || !fresh || w.acceptance != Acceptance::Open {
                    return false;
                }
                w.bump(Some(at));
                self.step_value(a)
            }
            KeyEvent::Release => {
                if w.remove(a).is_some_and(HoldClass::blocks) {
                    w.bump(Some(at));
                }
                false
            }
        }
    }

    /// The window gained or lost focus at `at`. A loss disarms every key
    /// down ([`HoldClass::FocusDisarmed`]) and clears an overflow; a return
    /// counts as activity, so the quiet wait runs from it. Either moves the
    /// revision; the same focus again changes nothing.
    pub fn focus(&self, focused: bool, at: TimeUs) {
        let mut w = self.window();
        w.normalize(self.record.opened());
        if w.focused == focused {
            return;
        }
        w.focused = focused;
        if focused {
            w.bump(Some(at));
        } else {
            for (_, class) in w.held.iter_mut().flatten() {
                *class = HoldClass::FocusDisarmed;
            }
            w.overflow = false;
            w.lost_since_reopen = true;
            w.focus_losses = w.focus_losses.wrapping_add(1);
            w.bump(None);
        }
        // Written before the window's other focus state, which the input
        // thread samples: an opening it is asked for sees the loss here.
        self.publish(&w);
    }

    /// A loading state begins at `at` (a chart chosen to play, a practice
    /// Start): every key down that would hold is held from before it
    /// ([`HoldClass::PreLoading`]), and acceptance is open.
    pub fn enter_loading(&self, at: TimeUs) {
        let mut w = self.window();
        w.epoch_start = at;
        for (_, class) in w.held.iter_mut().flatten() {
            if matches!(class, HoldClass::Fresh | HoldClass::GapDiscarded) {
                *class = HoldClass::PreLoading;
            }
        }
        w.acceptance = Acceptance::Open;
        w.lost_since_reopen = false;
        self.publish(&w);
    }

    /// A copy of the tuning, the step lock held: the values, and the
    /// activity record read as one snapshot (both sides' revisions, holds
    /// and last activity, overflow, focus and its losses). The reducer's
    /// count is 0: its owner adds it. Copies only: nothing allocates,
    /// waits or calls back.
    pub fn snapshot(&self) -> TuningSnapshot {
        let w = self.window();
        self.copy(&w)
    }

    /// [`snapshot`](Self::snapshot), the step lock held. A read of the
    /// record that stays unstable gives an unstable copy: never quiet,
    /// never unchanged.
    fn copy(&self, w: &Window) -> TuningSnapshot {
        let values = Some(self.get());
        let Some(read) = self.record.read() else {
            return TuningSnapshot {
                stable: false,
                values,
                revision: AcceptanceRevision {
                    window: w.revision,
                    input: u64::MAX,
                    reducer: 0,
                },
                held: true,
                overflow: w.overflow,
                focused: w.focused,
                focus_losses: w.focus_losses,
                last_activity: w.last,
            };
        };
        let (window, input) = (read.window, read.input);
        let last = match (window.last, input.last) {
            (Some(a), Some(b)) => Some(TimeUs(a.0.max(b.0))),
            (a, b) => a.or(b),
        };
        TuningSnapshot {
            stable: true,
            values,
            revision: AcceptanceRevision {
                window: window.revision,
                input: input.revision,
                reducer: 0,
            },
            held: window.held || input.held,
            overflow: window.overflow,
            focused: !window.unfocused,
            focus_losses: window.focus_losses,
            last_activity: last,
        }
    }

    /// Closes acceptance until candidate `generation`'s recording opens,
    /// and returns the tuning as it closed ([`snapshot`](Self::snapshot)).
    pub fn close(&self, generation: u32) -> TuningSnapshot {
        let mut w = self.window();
        w.acceptance = Acceptance::Closed {
            reopen_at: generation,
        };
        self.copy(&w)
    }

    /// The opening's revalidation, the step lock held: when the copy now
    /// is unchanged since `at_o1` at `now` (both sides' revisions,
    /// [`TuningSnapshot::unchanged_since`]; the reducer's is its owner's),
    /// acceptance closes until candidate `generation`'s recording opens.
    /// Whether it closed.
    pub fn close_unchanged(&self, at_o1: &TuningSnapshot, generation: u32, now: TimeUs) -> bool {
        let mut w = self.window();
        let mut copy = self.copy(&w);
        copy.revision.reducer = at_o1.revision.reducer;
        let unchanged = copy.unchanged_since(at_o1, now);
        if unchanged {
            w.acceptance = Acceptance::Closed {
                reopen_at: generation,
            };
        }
        unchanged
    }

    /// Reopens acceptance.
    pub fn reopen(&self) {
        let mut w = self.window();
        w.acceptance = Acceptance::Open;
        w.lost_since_reopen = false;
    }

    /// Whether focus was lost since acceptance last reopened (or the
    /// loading state began).
    pub fn focus_lost_since_reopen(&self) -> bool {
        self.window().lost_since_reopen
    }

    /// Whether the end screen shows: the input offset keys work.
    pub fn end_screen_shown(&self) -> bool {
        self.end_screen.load(Ordering::Acquire)
    }

    /// Acceptance as it stands, a closed one whose candidate opened read
    /// as open (tests).
    #[cfg(test)]
    pub fn acceptance(&self) -> Acceptance {
        let mut w = self.window();
        w.normalize(self.record.opened());
        w.acceptance
    }

    /// The settings keys down, with their classes (tests).
    #[cfg(test)]
    pub fn held(&self) -> Vec<(Adjust, HoldClass)> {
        self.window().held.iter().flatten().copied().collect()
    }

    /// Takes at most `n` keys down from now on (tests: an overflow).
    #[cfg(test)]
    pub fn set_capacity(&self, n: usize) {
        self.window().capacity = n.min(MAX_HELD);
    }

    /// The end screen shows: the input offset keys work.
    pub fn open_end_screen(&self) {
        self.end_screen.store(true, Ordering::Release);
    }

    /// The result screen is left (a play is about to start): the input
    /// offset keys stop working, and the values the play starts with are
    /// returned, in one step with respect to [`adjust`](Self::adjust) and
    /// [`key`](Self::key). An input offset key down now is dropped from the
    /// held set in the same step: its value stays, and its repeats and
    /// release, refused from here, count nothing (design.md "Play
    /// Lifecycle", tuning while loading).
    pub fn close_end_screen(&self) -> Tuning {
        let mut w = self.window();
        self.end_screen.store(false, Ordering::Release);
        for entry in &mut w.held {
            if entry.is_some_and(|(k, _)| k.is_input_offset()) {
                *entry = None;
            }
        }
        self.publish(&w);
        self.get()
    }
}

/// The settings keys' values as last delivered (to the audio thread and the
/// end screen): nothing, until the first delivery.
#[derive(Debug, Default)]
pub struct Delivered(Option<u64>);

impl Delivered {
    /// The values, if they changed since the last delivery or none was
    /// made yet.
    pub fn take_new(&mut self, tuning: &LiveTuning) -> Option<Tuning> {
        let version = tuning.version();
        if self.0 == Some(version) {
            return None;
        }
        self.0 = Some(version);
        Some(tuning.get())
    }
}

/// `device.toml` and `player.toml` as the settings keys and the select
/// screen's preset leave them.
#[derive(Debug)]
pub struct SettingsFiles {
    root: DataRoot,
    /// What the files hold: as loaded, then as last written.
    written: Settings,
    /// The play preset the select screen chose, written with the rest.
    preset: String,
    /// Why `player.toml` could not be written, until it is or holds what
    /// is wanted.
    player_error: Option<String>,
    /// The play adjustments' settings as the reducer last left them,
    /// written with the rest; as loaded until set.
    options: Option<game::PlayOptions>,
}

impl SettingsFiles {
    /// The files in `root`, holding `loaded`.
    pub fn new(root: DataRoot, loaded: Settings) -> Self {
        Self {
            root,
            preset: loaded.player.preset.clone(),
            written: loaded,
            player_error: None,
            options: None,
        }
    }

    /// The play adjustments' settings `player.toml` is to hold from now
    /// on: the ones every accepted change left ([`game::AdjustDriver::options`]).
    pub const fn set_options(&mut self, options: game::PlayOptions) {
        self.options = Some(options);
    }

    /// The play adjustments' settings `player.toml` is to hold: as set,
    /// else as loaded; `None` only for loaded ones out of range, which
    /// startup refuses.
    pub fn play_options(&self) -> Option<game::PlayOptions> {
        self.options.or_else(|| self.written.player.options().ok())
    }

    /// The data root the files are in (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub const fn root(&self) -> &DataRoot {
        &self.root
    }

    /// The play preset `player.toml` is to hold.
    pub fn preset(&self) -> &str {
        &self.preset
    }

    /// Why `player.toml` could not be written at the last write that
    /// tried, while it does not hold what is wanted.
    pub fn player_error(&self) -> Option<&str> {
        self.player_error.as_deref()
    }

    /// Chooses the play preset `name` and writes it at once, with the
    /// values `t` and the periods the audio output queues, as
    /// [`write`](Self::write) does (design.md "Settings Files").
    ///
    /// # Errors
    ///
    /// Why `player.toml` could not be written: the choice is kept and
    /// written at the next write.
    pub fn choose_preset(
        &mut self,
        name: &str,
        t: Tuning,
        queued_periods: u32,
    ) -> Result<(), String> {
        name.clone_into(&mut self.preset);
        self.write(t, queued_periods);
        match &self.player_error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Writes the values `t` and the periods the audio output queues into
    /// the file that holds each, atomically and only when it changes.
    /// Called when a play ends and when the run ends, never during a play.
    /// A file that cannot be written keeps its old content and is tried
    /// again at the next call. Returns the files written.
    pub fn write(&mut self, t: Tuning, queued_periods: u32) -> Vec<&'static str> {
        let mut next = self.written.clone();
        t.apply_to(&mut next);
        next.device.queued_periods = queued_periods;
        next.player.preset.clone_from(&self.preset);
        if let Some(options) = &self.options {
            next.player.set_options(options);
        }
        let mut wrote = Vec::new();
        if next.device != self.written.device {
            let path = self.root.device_settings();
            match save_settings(&path, &next.device) {
                Ok(()) => {
                    self.written.device = next.device;
                    wrote.push("device.toml");
                }
                Err(e) => eprintln!("{}: {}", path.display(), Chain(&e)),
            }
        }
        if next.player == self.written.player {
            self.player_error = None;
        } else {
            let path = self.root.player_settings();
            match save_settings(&path, &next.player) {
                Ok(()) => {
                    self.written.player = next.player;
                    self.player_error = None;
                    wrote.push("player.toml");
                }
                Err(e) => {
                    let why = Chain(&e).to_string();
                    eprintln!("{}: {why}", path.display());
                    self.player_error = Some(why);
                }
            }
        }
        for name in &wrote {
            println!("settings: wrote {name}");
        }
        wrote
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;
    use game::{Adjust, Tuning};

    use super::{Acceptance, Delivered, HoldClass, KeyEvent, LiveTuning, SettingsFiles};

    const S: i64 = 1_000_000;

    /// A6: after the opening's snapshot, a display offset press and
    /// release stamped before the last activity moves the window's
    /// revision by 2 and leaves the last activity: the revalidation finds
    /// the change and does not close, though the quiet wait would not.
    #[test]
    fn an_older_stamped_change_after_o1_retries() {
        let t = LiveTuning::new(start());
        t.enter_loading(TimeUs(0));
        assert!(t.key(Adjust::DisplayOffsetUp, KeyEvent::Press, TimeUs(5 * S)));
        assert!(!t.key(
            Adjust::DisplayOffsetUp,
            KeyEvent::Release,
            TimeUs(5 * S + 1)
        ));
        let at_o1 = t.snapshot();
        assert_eq!(at_o1.last_activity, Some(TimeUs(5 * S + 1)));
        assert!(t.key(Adjust::DisplayOffsetUp, KeyEvent::Press, TimeUs(4 * S)));
        assert!(!t.key(
            Adjust::DisplayOffsetUp,
            KeyEvent::Release,
            TimeUs(4 * S + 1)
        ));
        let now = t.snapshot();
        assert_eq!(now.revision.window, at_o1.revision.window + 2);
        assert_eq!(now.last_activity, at_o1.last_activity);
        assert_eq!(
            now.values.map(|v| v.display_offset_us),
            at_o1.values.map(|v| v.display_offset_us + 1_000)
        );
        assert!(now.quiet_at(TimeUs(6 * S + 1)), "quiet by the time stamps");
        assert!(!now.unchanged_since(&at_o1, TimeUs(6 * S + 1)));
        assert!(
            !t.close_unchanged(&at_o1, 3, TimeUs(6 * S + 1)),
            "a local retry"
        );
        assert_eq!(t.acceptance(), Acceptance::Open);
        // The next opening's snapshot closes.
        let again = t.snapshot();
        assert!(t.close_unchanged(&again, 3, TimeUs(6 * S + 1)));
        assert_eq!(t.acceptance(), Acceptance::Closed { reopen_at: 3 });
    }

    /// A8: a press that does not fit the held set is an overflow: it steps
    /// nothing, moves the revision and the last activity, and holds the
    /// loading state and the opening after every key is released, until
    /// focus loss clears it.
    #[test]
    fn an_overflowing_hold_blocks_the_opening() {
        let t = LiveTuning::new(start());
        t.set_capacity(1);
        assert!(t.key(Adjust::VolumeDown, KeyEvent::Press, TimeUs(S)));
        let before = t.snapshot();
        assert!(!t.key(Adjust::DisplayOffsetUp, KeyEvent::Press, TimeUs(2 * S)));
        let over = t.snapshot();
        assert!(over.overflow);
        assert_eq!(over.revision.window, before.revision.window + 1);
        assert_eq!(over.last_activity, Some(TimeUs(2 * S)));
        assert_eq!(
            over.values, before.values,
            "the overflowing press stepped nothing"
        );
        assert_eq!(t.held(), [(Adjust::VolumeDown, HoldClass::Fresh)]);
        t.key(Adjust::VolumeDown, KeyEvent::Release, TimeUs(3 * S));
        t.key(Adjust::DisplayOffsetUp, KeyEvent::Release, TimeUs(3 * S));
        let released = t.snapshot();
        assert!(t.held().is_empty());
        assert!(released.overflow && !released.held);
        assert!(!released.quiet_at(TimeUs(100 * S)));
        assert!(!t.close_unchanged(&released, 1, TimeUs(100 * S)));
        t.focus(false, TimeUs(4 * S));
        assert!(!t.snapshot().overflow, "focus loss clears it");
        t.focus(true, TimeUs(5 * S));
        let back = t.snapshot();
        assert!(!back.quiet_at(TimeUs(6 * S - 1)) && back.quiet_at(TimeUs(6 * S)));
        assert!(t.close_unchanged(&back, 1, TimeUs(6 * S)));
    }

    /// A4, a press routed after the loading state began but stamped
    /// before its epoch (a key event the window thread took late): held
    /// from before loading, so it holds the wait and steps nothing; its
    /// repeat neither; its release counts.
    #[test]
    fn a_press_stamped_before_the_epoch_is_held_from_before_loading() {
        let t = LiveTuning::new(start());
        t.enter_loading(TimeUs(2 * S));
        let began = t.snapshot();
        assert!(!t.key(Adjust::DisplayOffsetUp, KeyEvent::Press, TimeUs(S)));
        assert_eq!(t.held(), [(Adjust::DisplayOffsetUp, HoldClass::PreLoading)]);
        let pressed = t.snapshot();
        assert!(pressed.held, "it holds the wait");
        assert_eq!(pressed.values, began.values);
        assert_eq!(pressed.revision, began.revision);
        assert_eq!(pressed.last_activity, began.last_activity);
        assert!(!t.key(Adjust::DisplayOffsetUp, KeyEvent::Repeat, TimeUs(3 * S)));
        assert_eq!(t.snapshot(), pressed);
        assert!(!t.key(Adjust::DisplayOffsetUp, KeyEvent::Release, TimeUs(3 * S)));
        let released = t.snapshot();
        assert!(t.held().is_empty());
        assert!(!released.held);
        assert_eq!(released.values, began.values);
        assert_eq!(released.revision.window, began.revision.window + 1);
    }

    /// §2.10 R7 (finding 7): an input offset key held when the result is
    /// left is dropped from the held set as the keys are revoked, its value
    /// kept, no activity; the loading state that follows is not held, and
    /// the key's repeat and release change nothing.
    #[test]
    fn a_held_offset_key_is_dropped_at_the_revocation() {
        let t = LiveTuning::new(start());
        t.open_end_screen();
        assert!(t.key(Adjust::InputOffsetUp, KeyEvent::Press, TimeUs(S)));
        assert_eq!(t.held(), [(Adjust::InputOffsetUp, HoldClass::Fresh)]);
        let held = t.snapshot();
        assert!(held.held);
        assert_eq!(t.close_end_screen().input_offset_us, 3_000);
        assert!(t.held().is_empty());
        let revoked = t.snapshot();
        assert!(!revoked.held);
        assert_eq!(revoked.revision, held.revision, "no activity");
        t.enter_loading(TimeUs(2 * S));
        let loading = t.snapshot();
        assert!(loading.quiet_at(TimeUs(2 * S)));
        assert!(!t.key(Adjust::InputOffsetUp, KeyEvent::Repeat, TimeUs(3 * S)));
        assert!(!t.key(Adjust::InputOffsetUp, KeyEvent::Release, TimeUs(4 * S)));
        assert_eq!(t.snapshot(), loading);
        assert_eq!(t.get().input_offset_us, 3_000);
    }

    /// A9: an input offset key while the end screen does not show is
    /// refused: its press, repeat and release change no value, move no
    /// revision or activity, and hold nothing.
    #[test]
    fn a_refused_offset_key_is_not_activity() {
        let t = LiveTuning::new(start());
        t.enter_loading(TimeUs(0));
        let before = t.snapshot();
        for a in [Adjust::InputOffsetDown, Adjust::InputOffsetUp] {
            for e in [KeyEvent::Press, KeyEvent::Repeat, KeyEvent::Release] {
                assert!(!t.key(a, e, TimeUs(S)), "{a:?} {e:?}");
                assert!(t.held().is_empty(), "{a:?} {e:?}");
                assert_eq!(t.snapshot(), before, "{a:?} {e:?}");
            }
        }
        assert!(before.quiet_at(TimeUs(0)));
    }

    fn start() -> Tuning {
        Tuning {
            volume_percent: 100,
            display_offset_us: 0,
            input_offset_us: 2_000,
        }
    }

    #[test]
    fn input_offset_keys_work_on_the_end_screen_only() {
        let t = LiveTuning::new(start());
        assert!(!t.adjust(Adjust::InputOffsetUp));
        assert!(!t.adjust(Adjust::InputOffsetDown));
        assert_eq!((t.get(), t.version()), (start(), 0));
        t.open_end_screen();
        assert!(t.adjust(Adjust::InputOffsetUp));
        assert_eq!(t.get().input_offset_us, 3_000);
        assert_eq!(t.version(), 1);
        // Closing it returns the values the next play starts with, and
        // refuses input offset steps from then on.
        assert_eq!(t.close_end_screen().input_offset_us, 3_000);
        assert!(!t.adjust(Adjust::InputOffsetDown));
        assert!(!t.adjust(Adjust::InputOffsetUp));
        assert_eq!(t.close_end_screen().input_offset_us, 3_000);
        assert_eq!(t.get().input_offset_us, 3_000);
    }

    /// A volume changed before the game thread first looks (during
    /// startup, after the engine took the loaded volume) is delivered at
    /// the first look; then only a new change is.
    #[test]
    fn the_first_look_delivers_a_change_made_before_it() {
        let t = LiveTuning::new(start());
        assert!(t.adjust(Adjust::VolumeDown));
        let mut seen = Delivered::default();
        assert_eq!(seen.take_new(&t).map(|v| v.volume_percent), Some(95));
        assert_eq!(seen.take_new(&t), None);
        assert!(t.adjust(Adjust::VolumeDown));
        assert_eq!(seen.take_new(&t).map(|v| v.volume_percent), Some(90));
        assert_eq!(seen.take_new(&t), None);
    }

    /// A volume between whole percents plays as loaded until a volume key
    /// moves it; other keys leave it.
    #[test]
    fn the_loaded_volume_plays_until_a_volume_key_moves_it() {
        let quiet = Tuning {
            volume_percent: 0,
            ..start()
        };
        let t = LiveTuning::new(quiet).with_loaded_volume(0.004);
        assert_eq!(t.volume(), 0.004);
        assert!(t.adjust(Adjust::DisplayOffsetUp));
        assert_eq!(t.volume(), 0.004);
        assert!(t.adjust(Adjust::VolumeUp));
        assert_eq!(t.volume(), 0.05);
        assert!(t.adjust(Adjust::VolumeDown));
        assert_eq!(t.volume(), 0.0);
        // Without a loaded volume, the whole percent plays.
        assert_eq!(LiveTuning::new(start()).volume(), 1.0);
    }

    #[test]
    fn a_step_at_a_bound_changes_nothing() {
        let t = LiveTuning::new(start());
        // Volume is at unity already.
        assert!(!t.adjust(Adjust::VolumeUp));
        assert_eq!(t.version(), 0);
        assert!(t.adjust(Adjust::VolumeDown));
        assert!(t.adjust(Adjust::DisplayOffsetDown));
        assert_eq!(
            t.get(),
            Tuning {
                volume_percent: 95,
                display_offset_us: -1_000,
                ..start()
            }
        );
        assert_eq!(t.version(), 2);
    }

    /// Only a changed file is written, and one that cannot be written is
    /// written at the next call.
    #[test]
    fn a_failed_write_is_tried_again_and_unchanged_files_are_left() {
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().to_owned());
        let loaded = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let mut files = SettingsFiles::new(root.clone(), loaded.clone());
        let t = Tuning::of(&loaded);
        assert!(files.write(t, 2).is_empty());
        assert!(!root.device_settings().exists());
        // `player.toml` cannot be written: a directory is in the way.
        std::fs::create_dir_all(root.player_settings().join("x")).unwrap();
        assert!(files.choose_preset("lr2-style", t, 2).is_err());
        assert!(files.write(t, 2).is_empty());
        std::fs::remove_dir_all(root.player_settings()).unwrap();
        assert_eq!(files.write(t, 2), ["player.toml"]);
        assert!(files.write(t, 2).is_empty());
        let s = game::Settings::load(&root).unwrap();
        assert_eq!(s.player.preset, "lr2-style");
        assert!(!root.device_settings().exists());
        assert_eq!(
            files.write(t.adjust(Adjust::VolumeDown), 2),
            ["device.toml"]
        );
    }

    /// A chosen preset is written to `player.toml` at once, with the values
    /// as they stand, and later writes keep it.
    #[test]
    fn a_chosen_preset_is_written_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().to_owned());
        let loaded = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let mut files = SettingsFiles::new(root.clone(), loaded.clone());
        let t = Tuning::of(&loaded);
        assert_eq!(files.choose_preset("lr2-style", t, 2), Ok(()));
        let s = game::Settings::load(&root).unwrap();
        assert_eq!(s.player.preset, "lr2-style");
        assert_eq!(files.preset(), "lr2-style");
        let quieter = t.adjust(Adjust::VolumeDown);
        assert_eq!(files.write(quieter, 2), ["device.toml"]);
        let s = game::Settings::load(&root).unwrap();
        assert_eq!(s.player.preset, "lr2-style");
        assert_eq!(files.player_error(), None);
    }

    /// A write of changed values keeps `speculation` as `device.toml` was
    /// loaded with it: absent, `true` or `false`.
    #[test]
    fn a_tuning_write_keeps_speculation_as_read() {
        for speculation in [None, Some(true), Some(false)] {
            let dir = tempfile::tempdir().unwrap();
            let root = game::DataRoot::new(dir.path().to_owned());
            let loaded = game::Settings {
                device: game::DeviceSettings {
                    speculation,
                    ..game::DeviceSettings::default()
                },
                player: game::PlayerSettings::default(),
            };
            let mut files = SettingsFiles::new(root.clone(), loaded.clone());
            let quieter = Tuning::of(&loaded).adjust(Adjust::VolumeDown);
            assert_eq!(files.write(quieter, 3), ["device.toml"]);
            let text = std::fs::read_to_string(root.device_settings()).unwrap();
            assert_eq!(
                text.lines().any(|l| l.starts_with("speculation")),
                speculation.is_some(),
                "{text}"
            );
            let s = game::Settings::load(&root).unwrap();
            assert_eq!(s.device.speculation, speculation);
        }
    }

    /// The select screen's preset written to `player.toml` keeps the
    /// `[skins]` tables the file was loaded with, unchanged.
    #[test]
    fn a_chosen_preset_keeps_the_skin_choices() {
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().to_owned());
        std::fs::write(
            root.player_settings(),
            "[skins.play]\nroot = 'D:\\skins\\Mine'\nentry = 'play/play7.luaskin'\n\
             options = { \"Lane cover\" = \"On\" }\n\
             [skins.result]\nroot = 'D:/skins/R'\nentry = 'result.json'\n\
             files = { \"Background\" = \"b.png\" }\n",
        )
        .unwrap();
        let loaded = game::Settings::load(&root).unwrap();
        assert!(loaded.player.skins.play.is_some() && loaded.player.skins.result.is_some());
        let mut files = SettingsFiles::new(root.clone(), loaded.clone());
        let t = Tuning::of(&loaded);
        assert_eq!(files.choose_preset("lr2-style", t, 2), Ok(()));
        let s = game::Settings::load(&root).unwrap();
        assert_eq!(s.player.preset, "lr2-style");
        assert_eq!(s.player.skins, loaded.player.skins);
    }

    /// A chosen preset that cannot be written is kept, with the error, and
    /// written by the next ordinary write, which clears the error; choosing
    /// back the preset the file holds clears it too.
    #[test]
    fn a_preset_that_cannot_be_written_is_written_later() {
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().to_owned());
        let loaded = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let mut files = SettingsFiles::new(root.clone(), loaded.clone());
        let t = Tuning::of(&loaded);
        // `player.toml` cannot be written: a directory is in the way.
        std::fs::create_dir_all(root.player_settings().join("x")).unwrap();
        let Err(e) = files.choose_preset("tapestry", t, 2) else {
            panic!("written");
        };
        assert_eq!(files.player_error(), Some(e.as_str()));
        assert_eq!(files.preset(), "tapestry");
        std::fs::remove_dir_all(root.player_settings()).unwrap();
        assert_eq!(files.write(t, 2), ["player.toml"]);
        assert_eq!(files.player_error(), None);
        assert_eq!(
            game::Settings::load(&root).unwrap().player.preset,
            "tapestry"
        );
        // Failing again, then choosing back what the file holds.
        std::fs::remove_file(root.player_settings()).unwrap();
        std::fs::create_dir_all(root.player_settings().join("x")).unwrap();
        assert!(files.choose_preset("lr2-style", t, 2).is_err());
        assert_eq!(files.choose_preset("tapestry", t, 2), Ok(()));
        assert_eq!(files.player_error(), None);
    }

    /// A raised audio queue goes to `device.toml` alone; a write that fails
    /// is tried again at the next call, and the choice is not lost.
    #[test]
    fn a_raised_queue_is_written_to_device_toml_and_retried() {
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().to_owned());
        let loaded = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let mut files = SettingsFiles::new(root.clone(), loaded.clone());
        let t = Tuning::of(&loaded);
        // `device.toml` cannot be written: a directory is in the way.
        std::fs::create_dir_all(root.device_settings().join("x")).unwrap();
        assert!(files.write(t, 3).is_empty());
        std::fs::remove_dir_all(root.device_settings()).unwrap();
        assert_eq!(files.write(t, 3), ["device.toml"]);
        assert!(files.write(t, 3).is_empty());
        let s = game::Settings::load(&root).unwrap();
        assert_eq!(s.device.queued_periods, 3);
        assert!(!root.player_settings().exists());
        // Set back by hand and loaded again, the default is written as is.
        assert_eq!(files.write(t, 2), ["device.toml"]);
        assert_eq!(
            game::Settings::load(&root).unwrap().device.queued_periods,
            2
        );
    }
}
