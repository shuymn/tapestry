//! The pointer on the select screen (design.md "Input", **Pointer input**):
//! what the window, the game thread and the render thread share of it, the
//! window's capture of the mouse's messages, and what the game thread makes
//! of a press, a notch or a drag once the render thread has hit-tested it on
//! the last frame it built.
//!
//! The path: the window admits a press or a notch into the menu queue only
//! under an active [pointer word](PointerShared::word) (the selection
//! lifetime, the window's cancellation generation and an active bit),
//! stamped with that word; a focus loss advances the generation, so every
//! press, notch and drag observed before it, queued, answered or written,
//! acts no more. The game thread
//! hands each run of consecutive pointer entries to the render thread as
//! one [`PointerBatch`] and takes nothing more from the queue until it comes
//! back answered (the barrier); the render thread answers every slot on the
//! frame it just built ([`PointerShared::serve`]) and wakes the thread that
//! submitted it. A drag is never queued: the window counts the moves of
//! the current hold in the [drag cell](DragCell), and the render thread
//! hit-tests at most one drag a frame, of that hold's own movement, into a
//! one-command drag slot, which the game applies only once the press that
//! began its hold was dispatched.
//!
//! Everything here is fixed-size and written in place: no allocation on
//! any thread's pointer path.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, TryLockError};
use std::thread::Thread;

use skin_model::{
    Button, PointerHit, PressHit, RatioSource, RowRefKind, SearchBoxes, SliderCommand,
};

use crate::select::{MenuInput, MenuKey};
use crate::selection_skin::SelectionLogic;

/// The presses and notches one batch holds at most: the queue's pointer
/// bound ([`crate::select::QUEUED_POINTER`]), so one batch takes any run.
pub const BATCH: usize = crate::select::QUEUED_POINTER;

/// Where a pointer word's lifetime begins: its low 32 bits sit above the
/// window's cancellation generation (bits 1 to 31) and the active bit.
const LIFETIME_SHIFT: u32 = 32;

/// A pointer word's cancellation generation bits.
const GENERATION: u64 = (1 << LIFETIME_SHIFT) - 2;

/// The selection lifetime a pointer word names: its low 32 bits.
#[must_use]
pub const fn lifetime_of(word: u64) -> u64 {
    word >> LIFETIME_SHIFT
}

/// Whether a pointer word names selection lifetime `lifetime`.
#[must_use]
pub const fn names(word: u64, lifetime: u64) -> bool {
    lifetime_of(word) == lifetime & (u64::MAX >> LIFETIME_SHIFT)
}

/// The word [`PointerShared::publish`] makes for `lifetime` (tests).
#[cfg(test)]
#[must_use]
pub const fn word_of(lifetime: u64) -> u64 {
    lifetime << LIFETIME_SHIFT | 1
}

/// Whether a pointer word is active: selection is the screen in front.
#[must_use]
pub const fn is_active(word: u64) -> bool {
    word & 1 == 1
}

/// The position cell's value while no pointer is in the window: no window
/// position has an `x` of `i32::MIN`.
const NO_POSITION: u64 = 1 << 63;

/// What the last selection frame built, recorded right after its build:
/// what the render thread answers a batch and a drag on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Built {
    /// The update's selection lifetime.
    pub lifetime: u64,
    /// The update's revision.
    pub revision: u64,
    /// The list's revision the update's view was made from.
    pub list_revision: u64,
    /// The selectable index of the row drawn at the cursor; none when
    /// nothing is listed.
    pub cursor: Option<u32>,
    /// Whether the frame drew the skin: not the built-in page (no skin,
    /// another installation, or a refusal).
    pub skinned: bool,
}

/// A press or a notch as the game thread hands it to the render thread:
/// the pointer word it was observed under, and what it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerRequest {
    /// A press of `button` at `at` (window pixels, the target's).
    Press {
        word: u64,
        button: Button,
        at: [i32; 2],
    },
    /// Wheel notches, toward the user positive (a row down each).
    Wheel { word: u64, notches: i32 },
}

impl PointerRequest {
    /// The request a queued pointer entry makes; none for another input.
    #[must_use]
    pub const fn of(input: &MenuInput) -> Option<Self> {
        match *input {
            MenuInput::Pointer { button, at, word } => Some(Self::Press { word, button, at }),
            MenuInput::Wheel { notches, word } => Some(Self::Wheel { word, notches }),
            _ => None,
        }
    }

    /// The pointer word it was observed under.
    #[must_use]
    pub const fn word(self) -> u64 {
        match self {
            Self::Press { word, .. } | Self::Wheel { word, .. } => word,
        }
    }
}

/// One slot of a batch: the request, then its answer written in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerSlot {
    pub request: PointerRequest,
    /// What the press hit on the batch's build; none for a notch.
    pub hit: Option<PointerHit>,
    /// The search boxes whose last prepared bounds held the press,
    /// whatever it hit ([`PressHit::inside_search`]).
    pub inside_search: SearchBoxes,
}

impl PointerSlot {
    /// A slot holding nothing.
    pub const EMPTY: Self = Self {
        request: PointerRequest::Wheel {
            word: 0,
            notches: 0,
        },
        hit: None,
        inside_search: SearchBoxes::NONE,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchState {
    /// No batch in flight.
    Empty,
    /// Submitted, waiting for the render thread.
    Pending,
    /// Answered, waiting for the game thread.
    Answered,
}

/// The one hit batch in flight: made once, its slots written in place.
#[derive(Debug)]
pub struct PointerBatch {
    /// Moves on at each submission and each reset: an answer is taken
    /// only by the sequence it was submitted under.
    sequence: u64,
    /// The selection lifetime it was submitted in.
    lifetime: u64,
    state: BatchState,
    len: usize,
    /// The build its slots were answered on; none when no selection frame
    /// was built.
    built: Option<Built>,
    /// The thread to wake once it is answered.
    submitter: Option<Thread>,
    slots: [PointerSlot; BATCH],
}

impl Default for PointerBatch {
    fn default() -> Self {
        Self {
            sequence: 0,
            lifetime: 0,
            state: BatchState::Empty,
            len: 0,
            built: None,
            submitter: None,
            slots: [PointerSlot::EMPTY; BATCH],
        }
    }
}

/// What the game thread finds of the batch it submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// Not answered yet: the barrier holds.
    Waiting,
    /// Answered on `built`: its first `len` slots were copied out.
    Answered { built: Option<Built>, len: usize },
    /// Reset since (a lifetime changed): nothing to dispatch.
    Gone,
}

/// The pointer's drag cell: the held button and its latest point, the word
/// and scene of its press, the presses admitted when its hold began (the
/// hold's identity), and the moves of that hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DragCell {
    pub held: Option<Button>,
    pub at: [i32; 2],
    pub word: u64,
    pub scene: u64,
    /// Presses admitted to the queue when the hold began, its own included.
    pub hold: u64,
    /// Moves of this hold: none at its press, so no movement of an earlier
    /// hold is ever this one's.
    pub moves: u64,
}

/// The movement the render thread hit-tested last: the word and hold it
/// belonged to, and how many moves of that hold it had.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    word: u64,
    hold: u64,
    moves: u64,
}

/// A drag hit-tested on a build: the slider command, the hold it belongs
/// to and the build it was tested on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragCommand {
    /// The pointer word the hold was observed under.
    pub word: u64,
    /// [`DragCell::hold`]: applied only once that many presses were taken.
    pub hold: u64,
    pub built: Built,
    pub command: SliderCommand,
}

/// The one drag command not yet taken, and the game thread to wake when
/// one is written.
#[derive(Debug, Default)]
struct DragSlot {
    command: Option<DragCommand>,
    waker: Option<Thread>,
}

/// The pointer's state shared by the window, the game thread and the
/// render thread, made once at startup.
#[derive(Debug)]
pub struct PointerShared {
    /// `lifetime << 32 | generation << 1 | active`: published by the game
    /// thread at each selection entry (generation 0), its bit 0 cleared
    /// when selection ends, its generation advanced by the window at each
    /// focus loss ([`cancel`](Self::cancel)).
    word: AtomicU64,
    /// The pointer's latest window position, two `i32`s, or
    /// [`NO_POSITION`]: the hover's point, read each frame.
    position: AtomicU64,
    drag: Mutex<DragCell>,
    batch: Mutex<PointerBatch>,
    drag_slot: Mutex<DragSlot>,
    /// Presses and notches refused at the window: the queue held
    /// [`crate::select::QUEUED_POINTER`] pointer entries.
    dropped: AtomicU64,
}

impl Default for PointerShared {
    fn default() -> Self {
        Self {
            word: AtomicU64::new(0),
            position: AtomicU64::new(NO_POSITION),
            drag: Mutex::default(),
            batch: Mutex::default(),
            drag_slot: Mutex::default(),
            dropped: AtomicU64::new(0),
        }
    }
}

impl PointerShared {
    /// Selection of `lifetime` is the screen in front (the game thread).
    pub fn publish(&self, lifetime: u64) {
        self.word
            .store(lifetime.wrapping_shl(LIFETIME_SHIFT) | 1, Ordering::Release);
    }

    /// The window lost its focus (the window thread): the word's
    /// cancellation generation moves on, so every press, notch and drag
    /// observed before it, wherever it is (queued, in a batch, answered,
    /// in the drag slot), is of another word at dispatch and does nothing.
    /// A generation never repeats within a lifetime: once the last one is
    /// spent, a loss clears the active bit instead, and the pointer takes
    /// nothing until the next entry publishes a word.
    fn cancel(&self) {
        let _ = self
            .word
            .try_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                Some(if w & GENERATION == GENERATION {
                    w & !1
                } else {
                    w & !GENERATION | w.wrapping_add(2) & GENERATION
                })
            });
    }

    /// Selection ends: the window queues nothing until the next entry.
    pub fn close(&self) {
        self.word.fetch_and(!1, Ordering::AcqRel);
    }

    /// The pointer word now.
    pub fn word(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    /// Presses and notches refused so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn refused(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// The pointer's position in the window; none outside it.
    pub fn position(&self) -> Option<[i32; 2]> {
        let v = self.position.load(Ordering::Acquire);
        (v != NO_POSITION).then(|| {
            let [x0, x1, x2, x3, y0, y1, y2, y3] = v.to_be_bytes();
            [
                i32::from_be_bytes([x0, x1, x2, x3]),
                i32::from_be_bytes([y0, y1, y2, y3]),
            ]
        })
    }

    fn set_position(&self, at: Option<[i32; 2]>) {
        let v = at.map_or(NO_POSITION, |[x, y]| {
            let [x0, x1, x2, x3] = x.to_be_bytes();
            let [y0, y1, y2, y3] = y.to_be_bytes();
            u64::from_be_bytes([x0, x1, x2, x3, y0, y1, y2, y3])
        });
        self.position.store(v, Ordering::Release);
    }

    fn drag_cell(&self) -> std::sync::MutexGuard<'_, DragCell> {
        self.drag.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The drag cell as it is (tests).
    #[cfg(test)]
    pub fn drag_now(&self) -> DragCell {
        *self.drag_cell()
    }

    // The game thread's side of the batch.

    /// Submits a batch of the requests `next` gives, at most [`BATCH`],
    /// for selection `lifetime`, and the current thread to wake once it is
    /// answered; its sequence, or none when `next` gave nothing.
    pub fn submit(
        &self,
        lifetime: u64,
        mut next: impl FnMut() -> Option<PointerRequest>,
    ) -> Option<u64> {
        let mut b = self.batch.lock().unwrap_or_else(PoisonError::into_inner);
        let mut len = 0;
        for slot in &mut b.slots {
            let Some(request) = next() else {
                break;
            };
            *slot = PointerSlot {
                request,
                ..PointerSlot::EMPTY
            };
            len += 1;
        }
        if len == 0 {
            return None;
        }
        b.sequence = b.sequence.wrapping_add(1);
        b.lifetime = lifetime;
        b.state = BatchState::Pending;
        b.len = len;
        b.built = None;
        b.submitter = Some(std::thread::current());
        Some(b.sequence)
    }

    /// Takes batch `sequence`'s answers into `out` once answered.
    pub fn take_answers(&self, sequence: u64, out: &mut [PointerSlot; BATCH]) -> Taken {
        let mut b = self.batch.lock().unwrap_or_else(PoisonError::into_inner);
        if b.sequence != sequence || b.state == BatchState::Empty {
            return Taken::Gone;
        }
        if b.state == BatchState::Pending {
            return Taken::Waiting;
        }
        *out = b.slots;
        b.state = BatchState::Empty;
        b.submitter = None;
        Taken::Answered {
            built: b.built,
            len: b.len,
        }
    }

    /// The thread a drag command wakes once written: the current one (the
    /// game thread, at a selection's entry).
    pub fn wake_on_drag(&self) {
        self.drag_slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .waker = Some(std::thread::current());
    }

    /// A selection lifetime begins or ends: the batch in flight is
    /// abandoned, and an untaken drag with it.
    pub fn reset(&self) {
        {
            let mut b = self.batch.lock().unwrap_or_else(PoisonError::into_inner);
            b.sequence = b.sequence.wrapping_add(1);
            b.state = BatchState::Empty;
            b.len = 0;
            b.submitter = None;
        }
        self.drag_slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .command = None;
    }

    /// Takes the drag slot's command when its hold's press is the last of
    /// the `presses` taken out of the queue and dispatched: one whose press
    /// is still queued stays, and one a later press followed is dropped (a
    /// later press ended its hold).
    pub fn take_drag(&self, presses: u64) -> Option<DragCommand> {
        let mut slot = self
            .drag_slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match slot.command {
            Some(d) if d.hold == presses => slot.command.take(),
            Some(d) if d.hold < presses => {
                slot.command = None;
                None
            }
            _ => None,
        }
    }

    /// Writes a drag command, the latest replacing an untaken one (tests).
    #[cfg(test)]
    pub fn put_drag(&self, command: DragCommand) {
        self.drag_slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .command = Some(command);
    }

    // The render thread's side.

    /// Answers a pending batch on what this frame built (`built`, none
    /// when it built no selection frame), with `hit` for each request
    /// observed in the build's lifetime on a skin it drew, and wakes its
    /// submitter; then hit-tests the drag cell's latest point once if its
    /// hold moved since `seen`. Takes the batch only if it is free at once.
    pub fn serve(
        &self,
        built: Option<Built>,
        logic: Option<&SelectionLogic>,
        target: [u32; 2],
        seen: &mut Seen,
    ) {
        let skin = logic.filter(|_| built.is_some_and(|b| b.skinned));
        let drawn = |word: u64| built.is_some_and(|b| b.skinned && names(word, b.lifetime));
        self.answer(built, |request| match (request, skin) {
            (PointerRequest::Press { word, button, at }, Some(logic)) if drawn(word) => logic
                .bundle
                .skin()
                .hit_at(&logic.sel, point(at), target, button),
            _ => PressHit::NONE,
        });
        let Some((word, hold, at)) = self.moved(seen) else {
            return;
        };
        let (Some(built), Some(logic)) = (built, skin) else {
            return;
        };
        if !drawn(word) {
            return;
        }
        if let Some(command) = logic.bundle.skin().drag_at(&logic.sel, point(at), target) {
            let mut slot = self
                .drag_slot
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            slot.command = Some(DragCommand {
                word,
                hold,
                built,
                command,
            });
            // The game thread takes it at its next look, now.
            if let Some(t) = &slot.waker {
                t.unpark();
            }
        }
    }

    /// Answers a pending batch with `hit` for each request, on `built`,
    /// and wakes its submitter; whether one was answered.
    pub fn answer(
        &self,
        built: Option<Built>,
        mut hit: impl FnMut(PointerRequest) -> PressHit,
    ) -> bool {
        let mut b = match self.batch.try_lock() {
            Ok(b) => b,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        if b.state != BatchState::Pending {
            return false;
        }
        let len = b.len;
        for slot in b.slots.iter_mut().take(len) {
            let h = hit(slot.request);
            slot.hit = h.hit;
            slot.inside_search = h.inside_search;
        }
        b.built = built;
        b.state = BatchState::Answered;
        let submitter = b.submitter.take();
        drop(b);
        if let Some(t) = submitter {
            t.unpark();
        }
        true
    }

    /// The drag cell's word, hold and latest point when the hold it holds
    /// moved since `seen` while held under the current, active word: one
    /// drag a frame at most, at the latest point. Only that hold's own
    /// movement counts, and a press begins its hold with none.
    fn moved(&self, seen: &mut Seen) -> Option<(u64, u64, [i32; 2])> {
        let cell = *self.drag_cell();
        let latest = Seen {
            word: cell.word,
            hold: cell.hold,
            moves: cell.moves,
        };
        if cell.moves == 0 || latest == *seen {
            return None;
        }
        *seen = latest;
        let word = self.word();
        (cell.held.is_some() && cell.word == word && is_active(word))
            .then_some((cell.word, cell.hold, cell.at))
    }
}

/// A window position as a target point: the window's client pixels are
/// the surface's.
#[expect(
    clippy::cast_precision_loss,
    reason = "a window position is far inside f32's exact integers"
)]
pub const fn point(at: [i32; 2]) -> [f32; 2] {
    let [x, y] = at;
    [x as f32, y as f32]
}

/// The game thread's side of the pointer: the shared state, the batch it
/// has in flight, if any (the barrier), and an answered batch's answers
/// not yet dispatched (a pick left them, and a refused pick resumes them).
#[derive(Debug)]
pub struct PointerPass {
    pub shared: std::sync::Arc<PointerShared>,
    /// The sequence of the batch in flight.
    pub flight: Option<u64>,
    answers: [PointerSlot; BATCH],
    /// The build the answers were made on.
    built: Option<Built>,
    /// The answers taken, and the next to dispatch.
    len: usize,
    next: usize,
    /// The search box a press gave the search focus to, by its order among
    /// the skin's search texts: the box whose last bounds a press must lie
    /// in to keep the focus. The game thread knows the focus, the render
    /// thread each box's bounds; each answer carries which boxes held the
    /// press ([`PointerSlot::inside_search`]), so the two meet here without
    /// either reading the other's state. `None` for focus another input
    /// gave (Tab), which every box holds, and cleared at every focus change
    /// and every entry.
    pub search_box: Option<u8>,
    /// While set, each input the pass dispatched, with the cursor after
    /// it (tests).
    #[cfg(test)]
    pub log: Option<Vec<(Dispatched, usize)>>,
}

/// An input a pass dispatched, as [`PointerPass::log`] records it (tests).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatched {
    /// A queued input that is not the pointer's.
    Input,
    Press(PointerAct),
    Notch(PointerAct),
    Drag(PointerAct),
}

impl PointerPass {
    /// The game thread's side of `shared`.
    #[must_use]
    pub const fn new(shared: std::sync::Arc<PointerShared>) -> Self {
        Self {
            shared,
            flight: None,
            answers: [PointerSlot::EMPTY; BATCH],
            built: None,
            len: 0,
            next: 0,
            search_box: None,
            #[cfg(test)]
            log: None,
        }
    }

    /// Selection of `lifetime` is entered on the current thread: the window
    /// takes the pointer for it, nothing of an earlier lifetime in flight,
    /// and a drag wakes this thread.
    pub fn enter(&mut self, lifetime: u64) {
        self.abandon();
        self.search_box = None;
        self.shared.reset();
        self.shared.wake_on_drag();
        self.shared.publish(lifetime);
    }

    /// Selection is left: the window takes nothing more, and nothing in
    /// flight or answered is dispatched.
    pub fn leave(&mut self) {
        self.abandon();
        self.shared.close();
        self.shared.reset();
    }

    fn abandon(&mut self) {
        self.flight = None;
        self.len = 0;
        self.next = 0;
    }

    /// Whether nothing is in flight and every answer was dispatched
    /// (dev-tools tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub const fn settled(&self) -> bool {
        self.flight.is_none() && self.next >= self.len
    }

    /// Takes the batch in flight's answers once the render thread answered
    /// it; [`Taken::Waiting`] while it has not.
    pub fn take(&mut self) -> Taken {
        let Some(sequence) = self.flight else {
            return Taken::Gone;
        };
        let taken = self.shared.take_answers(sequence, &mut self.answers);
        match taken {
            Taken::Waiting => {}
            Taken::Gone => self.flight = None,
            Taken::Answered { built, len } => {
                self.flight = None;
                self.built = built;
                self.len = len;
                self.next = 0;
            }
        }
        taken
    }

    /// The next answer not yet dispatched, and the build it was made on;
    /// it counts as dispatched from here.
    pub fn next_answer(&mut self) -> Option<(PointerSlot, Option<Built>)> {
        let slot = *self.answers.get(self.next.min(self.len))?;
        if self.next >= self.len {
            return None;
        }
        self.next += 1;
        Some((slot, self.built))
    }
}

impl Default for PointerPass {
    fn default() -> Self {
        Self::new(std::sync::Arc::default())
    }
}

/// The window's capture of the mouse's messages: the held button with the
/// pointer word and scene of its press, the pointer's position and the
/// wheel's fraction of a notch. The window calls it with the pointer word
/// and the scene as it reads them at each event; it queues through
/// `admit`, which returns the presses admitted so far, or none when the
/// queue refused the entry.
#[derive(Debug, Default)]
pub struct Capture {
    held: Option<Held>,
    position: Option<[i32; 2]>,
    /// Lines of wheel not yet a whole notch, and the word they came under.
    wheel: (f32, u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    button: Button,
    word: u64,
    scene: u64,
}

impl Capture {
    /// Drops a held button whose word or scene is not `word` and `scene`
    /// any more, or when `word` is inactive; whether the pointer is taken
    /// now. The position is kept either way: a press after the next entry
    /// is at the point the pointer rests on, and the hover draws there.
    fn check(&mut self, shared: &PointerShared, word: u64, scene: u64) -> bool {
        let active = is_active(word);
        if self
            .held
            .is_some_and(|h| !active || h.word != word || h.scene != scene)
        {
            self.drop_held(shared);
        }
        if self.wheel.1 != word {
            self.wheel = (0.0, word);
        }
        active
    }

    fn drop_held(&mut self, shared: &PointerShared) {
        self.held = None;
        shared.drag_cell().held = None;
    }

    /// The pointer moved to `at` in the window: the hover's point, and a
    /// drag while a button is held.
    pub fn moved(&mut self, shared: &PointerShared, at: [i32; 2], word: u64, scene: u64) {
        let active = self.check(shared, word, scene);
        self.position = Some(at);
        shared.set_position(Some(at));
        if active && self.held.is_some() {
            let mut cell = shared.drag_cell();
            cell.at = at;
            cell.moves = cell.moves.wrapping_add(1).max(1);
        }
    }

    /// The pointer left the window: no hover. A held button stays held,
    /// as the window keeps the mouse captured while it is.
    pub fn left(&mut self, shared: &PointerShared, word: u64, scene: u64) {
        self.check(shared, word, scene);
        self.position = None;
        shared.set_position(None);
    }

    /// `button` pressed or released: a press is queued at the pointer's
    /// position and, admitted, held until released or until its word or
    /// scene changes; a refused press is counted and holds nothing.
    pub fn button(
        &mut self,
        shared: &PointerShared,
        button: Button,
        pressed: bool,
        word: u64,
        scene: u64,
        admit: impl FnOnce(MenuInput) -> Option<u64>,
    ) {
        if !self.check(shared, word, scene) {
            return;
        }
        if !pressed {
            if self.held.is_some_and(|h| h.button == button) {
                self.drop_held(shared);
            }
            return;
        }
        let Some(at) = self.position else {
            return;
        };
        match admit(MenuInput::Pointer { button, at, word }) {
            Some(hold) => {
                self.held = Some(Held {
                    button,
                    word,
                    scene,
                });
                *shared.drag_cell() = DragCell {
                    held: Some(button),
                    at,
                    word,
                    scene,
                    hold,
                    moves: 0,
                };
            }
            None => {
                shared.refused();
                self.drop_held(shared);
            }
        }
    }

    /// The wheel turned `lines` (away from the user positive, fractions
    /// summed): each whole notch is queued, a row down toward the user.
    pub fn wheel(
        &mut self,
        shared: &PointerShared,
        lines: f32,
        word: u64,
        scene: u64,
        admit: impl FnOnce(MenuInput) -> Option<u64>,
    ) {
        if !self.check(shared, word, scene) {
            return;
        }
        let sum = self.wheel.0 + lines;
        let whole = sum.trunc();
        self.wheel.0 = sum - whole;
        // Saturating: a turn of 2^31 lines is no wheel's.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a whole number, saturated at i32's ends"
        )]
        let notches = (-whole) as i32;
        if notches != 0 && admit(MenuInput::Wheel { notches, word }).is_none() {
            shared.refused();
        }
    }

    /// The window's focus changed to `focused` (the window thread, every
    /// focus change the window reports): a loss first cancels the pointer
    /// ([`focus_lost`](Self::focus_lost)), and only then does `publish`
    /// tell the rest ([`Controls::focus`](crate::controls::Controls::focus),
    /// which wakes the game thread), so a thread that sees the loss never
    /// finds an input observed before it still current.
    pub fn focus_changed(
        &mut self,
        shared: &PointerShared,
        focused: bool,
        publish: impl FnOnce(bool),
    ) {
        if !focused {
            self.focus_lost(shared);
        }
        publish(focused);
    }

    /// The window lost its focus: nothing stays held, no hover, and
    /// nothing observed before acts ([`PointerShared::cancel`]).
    fn focus_lost(&mut self, shared: &PointerShared) {
        shared.cancel();
        self.drop_held(shared);
        self.position = None;
        self.wheel.0 = 0.0;
        shared.set_position(None);
    }
}

/// What the game thread knows of selection when it dispatches a pointer
/// input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The selection lifetime (`SkinFeed::lifetime`).
    pub lifetime: u64,
    /// The pointer word now.
    pub word: u64,
    /// The list's revision now.
    pub list_revision: u64,
    /// The selectable rows.
    pub count: usize,
    /// The cursor's selectable index.
    pub cursor: usize,
    pub searching: bool,
    /// The search box a press gave the focus to, by its order among the
    /// skin's search texts; `None` while not searching or for focus the
    /// keyboard gave, which every box holds ([`SearchBoxes::outside`]).
    pub search_box: Option<u8>,
    /// The IME composes (a preedit is shown).
    pub composing: bool,
    /// A panel is held.
    pub panel: bool,
}

/// What a pointer input does on the select screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerAct {
    Nothing,
    /// Acts as this key.
    Key(MenuKey),
    /// Moves the cursor to this selectable index.
    MoveTo(usize),
    /// Moves the cursor this many rows, down positive.
    Rows(i32),
}

/// A press's effect: the search box's focus taken away first, then `act`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub unfocus: bool,
    pub act: PointerAct,
}

const NOTHING: Resolved = Resolved {
    unfocus: false,
    act: PointerAct::Nothing,
};

/// Whether an input observed under `word`, tested on `built`, acts now:
/// one lifetime throughout, still in front, on a frame that drew the skin,
/// and no focus lost since (the word's generation).
fn current(word: u64, built: Option<Built>, facts: &Facts) -> Option<Built> {
    built.filter(|b| {
        b.skinned
            && names(word, b.lifetime)
            && b.lifetime == facts.lifetime
            && facts.word == word
            && is_active(word)
    })
}

/// What `request`, answered with `press` on `built`, does on the select screen
/// as `facts` and `kind_at` (the row kind at a selectable index) have it
/// (design.md "Input", **Pointer input**).
pub fn resolve(
    request: PointerRequest,
    press: PressHit,
    built: Option<Built>,
    facts: &Facts,
    kind_at: impl Fn(usize) -> Option<RowRefKind>,
) -> Resolved {
    let Some(built) = current(request.word(), built, facts) else {
        return NOTHING;
    };
    let hit = press.hit;
    if facts.composing {
        return NOTHING;
    }
    let button = match request {
        PointerRequest::Wheel { notches, .. } => {
            let act = if facts.panel || notches == 0 {
                PointerAct::Nothing
            } else {
                PointerAct::Rows(notches)
            };
            return Resolved {
                unfocus: false,
                act,
            };
        }
        PointerRequest::Press { button, .. } => button,
    };
    if facts.panel {
        // Under a panel only the order button acts.
        return match hit {
            Some(PointerHit::Action { event: 12, arg }) => Resolved {
                unfocus: false,
                act: order(arg),
            },
            _ => NOTHING,
        };
    }
    // A press outside the focused box's last bounds takes its focus
    // whatever it hits, drawn or hidden since, and one inside keeps it even
    // when a target drawn over the box takes the press: the source commits
    // the focused text outside its own bounds before it walks the targets
    // (`skin/Skin.java:394-397`, `skin/SkinTextInput.java:77-81`). Another
    // box hit then takes the focus.
    let unfocus = facts.searching && press.inside_search.outside(facts.search_box);
    let act = match hit {
        Some(PointerHit::Search { .. }) if facts.searching && !unfocus => PointerAct::Nothing,
        Some(PointerHit::Search { .. }) => PointerAct::Key(MenuKey::Tab),
        Some(PointerHit::Row { row, button }) => {
            let index = usize::try_from(row.index).unwrap_or(usize::MAX);
            let valid = row.list_revision == facts.list_revision
                && index < facts.count
                && kind_at(index) == Some(row.kind);
            match (valid, button) {
                (false, _) => PointerAct::Nothing,
                (true, Button::Left) if index == facts.cursor && drawn_cursor(&built, facts) => {
                    PointerAct::Key(MenuKey::Enter)
                }
                (true, Button::Left) => PointerAct::MoveTo(index),
                (true, _) => PointerAct::Key(MenuKey::Escape),
            }
        }
        Some(PointerHit::Action { event: 12, arg }) => order(arg),
        Some(PointerHit::Action {
            event: event @ (15 | 315),
            ..
        }) => {
            let chart = kind_at(facts.cursor) == Some(RowRefKind::Chart);
            if chart && drawn_cursor(&built, facts) {
                PointerAct::Key(if event == 15 {
                    MenuKey::Enter
                } else {
                    MenuKey::Practice
                })
            } else {
                PointerAct::Nothing
            }
        }
        // Every other action and slider takes the press and does nothing.
        Some(PointerHit::Action { .. }) => PointerAct::Nothing,
        Some(PointerHit::Slider(command)) => list_slider(command, &built, facts),
        None if button == Button::Right => PointerAct::Key(MenuKey::Escape),
        None => PointerAct::Nothing,
    };
    Resolved { unfocus, act }
}

/// What a drag does: the list slider moves the cursor as a press on it;
/// nothing else, under a panel or while the IME composes.
pub fn resolve_drag(drag: &DragCommand, facts: &Facts) -> PointerAct {
    match current(drag.word, Some(drag.built), facts) {
        Some(built) if !facts.composing && !facts.panel => list_slider(drag.command, &built, facts),
        _ => PointerAct::Nothing,
    }
}

/// The order button: a step as Right for a positive argument and Left for
/// a negative one.
const fn order(arg: i8) -> PointerAct {
    if arg > 0 {
        PointerAct::Key(MenuKey::Right)
    } else if arg < 0 {
        PointerAct::Key(MenuKey::Left)
    } else {
        PointerAct::Nothing
    }
}

/// Whether the build drew the cursor where it is now, on the current list.
fn drawn_cursor(built: &Built, facts: &Facts) -> bool {
    built.list_revision == facts.list_revision
        && built
            .cursor
            .and_then(|c| usize::try_from(c).ok())
            .is_some_and(|c| c == facts.cursor)
}

/// The list slider at `command`'s ratio `v`, drawn on the current list:
/// the cursor to `round((count − 1) · v)`, ties up; any other slider takes
/// the input and does nothing.
fn list_slider(command: SliderCommand, built: &Built, facts: &Facts) -> PointerAct {
    if command.source != RatioSource::ListPosition
        || built.list_revision != facts.list_revision
        || facts.count < 2
    {
        return PointerAct::Nothing;
    }
    let last = u128::try_from(facts.count - 1).unwrap_or(u128::MAX);
    let n = u128::from(command.value.numerator);
    let d = u128::from(command.value.denominator).max(1);
    let index = (last.saturating_mul(n).saturating_mul(2) + d) / (2 * d);
    let index = usize::try_from(index.min(last)).unwrap_or(usize::MAX);
    PointerAct::MoveTo(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skin_model::{Ratio, RowRef};

    /// An admission that takes everything: the presses so far.
    fn admitting<'a>(
        presses: &'a std::cell::Cell<u64>,
        queued: &'a std::cell::RefCell<Vec<MenuInput>>,
    ) -> impl FnOnce(MenuInput) -> Option<u64> + 'a {
        move |input| {
            if matches!(input, MenuInput::Pointer { .. }) {
                presses.set(presses.get() + 1);
            }
            queued.borrow_mut().push(input);
            Some(presses.get())
        }
    }

    const W7: u64 = word_of(7);
    const W8: u64 = word_of(8);

    #[test]
    fn the_word_is_the_lifetime_its_generation_and_its_active_bit() {
        let s = PointerShared::default();
        assert!(!is_active(s.word()));
        s.publish(7);
        assert_eq!(s.word(), W7);
        assert_eq!(lifetime_of(s.word()), 7);
        // A focus loss: another word of the same lifetime, still active.
        s.cancel();
        let lost = s.word();
        assert_ne!(lost, W7);
        assert!(names(lost, 7) && is_active(lost));
        // The last generation spent: no wrap, the word latches inactive.
        s.word.store(W7 | GENERATION, Ordering::Release);
        s.cancel();
        assert_eq!(s.word(), (W7 | GENERATION) & !1);
        // A lifetime past 32 bits is named by its low ones.
        assert!(names(W7, u64::from(u32::MAX) + 8));
        s.publish(8);
        s.publish(7);
        s.close();
        assert_eq!(s.word(), W7 & !1);
        assert!(!is_active(s.word()));
    }

    #[test]
    fn the_position_cell_keeps_negative_points_and_none() {
        let s = PointerShared::default();
        assert_eq!(s.position(), None);
        s.set_position(Some([-5, 1_079]));
        assert_eq!(s.position(), Some([-5, 1_079]));
        s.set_position(Some([-1, -1]));
        assert_eq!(s.position(), Some([-1, -1]));
        s.set_position(None);
        assert_eq!(s.position(), None);
    }

    #[test]
    fn an_inactive_word_queues_nothing_and_drops_the_hold() {
        let s = PointerShared::default();
        let (presses, queued) = (std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
        let mut c = Capture::default();
        c.moved(&s, [10, 20], W7 & !1, 0);
        // The position is followed, nothing queued.
        assert_eq!(s.position(), Some([10, 20]));
        c.button(
            &s,
            Button::Left,
            true,
            W7 & !1,
            0,
            admitting(&presses, &queued),
        );
        c.wheel(&s, -1.0, W7 & !1, 0, admitting(&presses, &queued));
        assert!(queued.borrow().is_empty());
        // Active: a press holds.
        c.moved(&s, [10, 20], W7, 0);
        c.button(&s, Button::Left, true, W7, 0, admitting(&presses, &queued));
        assert_eq!(s.drag_now().held, Some(Button::Left));
        assert_eq!(s.drag_now().hold, 1);
        // The word closes: the next event drops the hold; the position
        // stays followed.
        c.moved(&s, [11, 20], W7 & !1, 0);
        assert_eq!(s.drag_now().held, None);
        assert_eq!(s.position(), Some([11, 20]));
        assert_eq!(queued.borrow().len(), 1);
    }

    /// Selection left and entered again with the pointer resting where it
    /// moved meanwhile: a press without a move is queued there, and the
    /// hover's point is that one; only leaving the window or a focus loss
    /// forgets it.
    #[test]
    fn the_position_outlives_an_inactive_word() {
        let s = PointerShared::default();
        let (presses, queued) = (std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
        let mut c = Capture::default();
        c.moved(&s, [1, 1], W7, 0);
        // Selection left (the decision screen, a play): the pointer moves.
        c.moved(&s, [30, 40], W7 & !1, 0);
        c.button(
            &s,
            Button::Left,
            true,
            W7 & !1,
            0,
            admitting(&presses, &queued),
        );
        c.button(
            &s,
            Button::Left,
            false,
            W7 & !1,
            0,
            admitting(&presses, &queued),
        );
        assert!(queued.borrow().is_empty());
        // Entered again (lifetime 8, another scene): the same point.
        assert_eq!(s.position(), Some([30, 40]));
        c.button(&s, Button::Left, true, W8, 1, admitting(&presses, &queued));
        assert_eq!(
            *queued.borrow(),
            [MenuInput::Pointer {
                button: Button::Left,
                at: [30, 40],
                word: W8
            }]
        );
        c.left(&s, W8, 1);
        assert_eq!(s.position(), None);
        c.moved(&s, [5, 5], W8, 1);
        c.focus_lost(&s);
        assert_eq!(s.position(), None);
    }

    #[test]
    fn a_hold_counts_its_moves_only_while_its_word_and_scene_stand() {
        let s = PointerShared::default();
        let (presses, queued) = (std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
        let mut c = Capture::default();
        c.moved(&s, [1, 1], W7, 3);
        c.button(&s, Button::Left, true, W7, 3, admitting(&presses, &queued));
        // A press and a still hold move nothing.
        assert_eq!(s.drag_now().moves, 0);
        c.moved(&s, [2, 1], W7, 3);
        c.moved(&s, [3, 1], W7, 3);
        let cell = s.drag_now();
        assert_eq!((cell.moves, cell.at), (2, [3, 1]));
        // Another scene (a focus loss elsewhere): dropped, and a move after
        // it drags nothing until a fresh press.
        c.moved(&s, [4, 1], W7, 4);
        assert_eq!(s.drag_now().held, None);
        assert_eq!(s.drag_now().moves, 2);
        // A missed inactive interval: another lifetime, active.
        c.button(&s, Button::Left, true, W7, 4, admitting(&presses, &queued));
        assert_eq!(s.drag_now().held, Some(Button::Left));
        // A fresh press begins its hold with no movement.
        assert_eq!(s.drag_now().moves, 0);
        c.moved(&s, [5, 1], W8, 4);
        assert_eq!(s.drag_now().held, None);
        // A release of another button keeps the hold; its own drops it.
        c.button(&s, Button::Right, true, W8, 4, admitting(&presses, &queued));
        c.button(&s, Button::Left, false, W8, 4, |_| None);
        assert_eq!(s.drag_now().held, Some(Button::Right));
        c.button(&s, Button::Right, false, W8, 4, |_| None);
        assert_eq!(s.drag_now().held, None);
        assert_eq!(s.dropped(), 0);
    }

    /// A generation never repeats within a lifetime: past the last one a
    /// focus loss latches the word inactive, a press is not taken and an
    /// input of any earlier word stays stale, until the next entry.
    #[test]
    fn a_spent_generation_latches_the_pointer_until_the_next_entry() {
        let s = PointerShared::default();
        let (presses, queued) = (std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
        let mut c = Capture::default();
        // Lifetime 7, one loss short of its last generation.
        let next_to_last = W7 | (GENERATION - 2);
        s.word.store(next_to_last, Ordering::Release);
        c.focus_changed(&s, false, |_| {});
        let last = s.word();
        assert_eq!(last, W7 | GENERATION);
        assert!(is_active(last));
        c.focus_changed(&s, false, |_| {});
        let latched = s.word();
        assert!(!is_active(latched), "{latched:#x}");
        assert_eq!(lifetime_of(latched), 7);
        for earlier in [W7, next_to_last, last] {
            assert_ne!(latched, earlier);
        }
        // Further losses keep it latched; nothing is queued under it.
        c.focus_changed(&s, false, |_| {});
        assert_eq!(s.word(), latched);
        c.moved(&s, [1, 1], s.word(), 0);
        c.button(
            &s,
            Button::Left,
            true,
            s.word(),
            0,
            admitting(&presses, &queued),
        );
        assert!(queued.borrow().is_empty());
        // The next entry takes the pointer again.
        s.publish(8);
        assert_eq!(s.word(), W8);
        c.button(&s, Button::Left, true, W8, 0, admitting(&presses, &queued));
        assert_eq!(queued.borrow().len(), 1);
    }

    /// The window's focus change cancels the pointer before it tells the
    /// rest: what `publish` runs (the game thread, woken) already finds the
    /// word moved on.
    #[test]
    fn a_focus_loss_cancels_the_pointer_before_it_is_published() {
        let s = PointerShared::default();
        s.publish(7);
        let mut c = Capture::default();
        let mut seen = None;
        c.focus_changed(&s, false, |focused| seen = Some((focused, s.word())));
        let (focused, word) = seen.unwrap();
        assert!(!focused);
        assert_ne!(word, W7, "published before the cancel");
        // A return cancels nothing.
        c.focus_changed(&s, true, |focused| seen = Some((focused, s.word())));
        assert_eq!(seen, Some((true, word)));
    }

    #[test]
    fn focus_loss_drops_the_hold_and_the_hover() {
        let s = PointerShared::default();
        let (presses, queued) = (std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
        let mut c = Capture::default();
        c.moved(&s, [1, 1], W7, 0);
        c.button(&s, Button::Left, true, W7, 0, admitting(&presses, &queued));
        s.publish(7);
        c.focus_lost(&s);
        // What was observed under W7 acts no more.
        assert_ne!(s.word(), W7);
        assert_eq!(s.drag_now().held, None);
        assert_eq!(s.position(), None);
        // Back, moved with the button still down: no drag.
        let moves = s.drag_now().moves;
        c.moved(&s, [9, 9], W7, 1);
        assert_eq!(s.drag_now().moves, moves);
        // A press with no position known is not queued.
        let mut fresh = Capture::default();
        fresh.button(&s, Button::Left, true, W7, 1, admitting(&presses, &queued));
        assert_eq!(queued.borrow().len(), 1);
    }

    #[test]
    fn a_refused_press_is_counted_and_holds_nothing() {
        let s = PointerShared::default();
        let mut c = Capture::default();
        c.moved(&s, [1, 1], W7, 0);
        c.button(&s, Button::Left, true, W7, 0, |_| None);
        assert_eq!(s.dropped(), 1);
        assert_eq!(s.drag_now().held, None);
        c.wheel(&s, 1.0, W7, 0, |_| None);
        assert_eq!(s.dropped(), 2);
    }

    #[test]
    fn the_wheel_keeps_its_fraction_until_a_whole_notch() {
        let s = PointerShared::default();
        let (presses, queued) = (std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
        let mut c = Capture::default();
        for _ in 0..3 {
            c.wheel(&s, -0.4, W7, 0, admitting(&presses, &queued));
        }
        // −1.2 lines: one notch toward the user, a row down.
        assert_eq!(
            *queued.borrow(),
            [MenuInput::Wheel {
                notches: 1,
                word: W7
            }]
        );
        c.wheel(&s, 2.9, W7, 0, admitting(&presses, &queued));
        // −0.2 + 2.9 = 2.7: two up.
        assert_eq!(
            queued.borrow().last(),
            Some(&MenuInput::Wheel {
                notches: -2,
                word: W7
            })
        );
        // Another lifetime starts the fraction afresh: 0.7 + 0.5 is not
        // carried.
        c.wheel(&s, 0.5, W8, 0, admitting(&presses, &queued));
        assert_eq!(queued.borrow().len(), 2);
        c.wheel(&s, 0.5, W8, 0, admitting(&presses, &queued));
        assert_eq!(queued.borrow().len(), 3);
        c.focus_lost(&s);
        c.wheel(&s, 0.6, W8, 0, admitting(&presses, &queued));
        assert_eq!(queued.borrow().len(), 3);
    }

    fn built(lifetime: u64) -> Built {
        Built {
            lifetime,
            revision: 4,
            list_revision: 2,
            cursor: Some(1),
            skinned: true,
        }
    }

    fn facts() -> Facts {
        Facts {
            lifetime: 7,
            word: W7,
            list_revision: 2,
            count: 5,
            cursor: 1,
            searching: false,
            search_box: None,
            composing: false,
            panel: false,
        }
    }

    fn kinds(i: usize) -> Option<RowRefKind> {
        [
            RowRefKind::All,
            RowRefKind::Folder,
            RowRefKind::Chart,
            RowRefKind::Chart,
            RowRefKind::Chart,
        ]
        .get(i)
        .copied()
    }

    fn press(button: Button) -> PointerRequest {
        PointerRequest::Press {
            word: W7,
            button,
            at: [0, 0],
        }
    }

    fn row(index: u32, kind: RowRefKind, button: Button) -> Option<PointerHit> {
        Some(PointerHit::Row {
            row: RowRef {
                list_revision: 2,
                index,
                kind,
            },
            button,
        })
    }

    /// The only search box.
    const SEARCH: Option<PointerHit> = Some(PointerHit::Search { index: 0 });

    /// `hit` answered as a press inside the search box exactly when it hit
    /// the box.
    fn on(hit: Option<PointerHit>) -> PressHit {
        PressHit {
            hit,
            inside_search: if hit == SEARCH {
                SearchBoxes(1)
            } else {
                SearchBoxes::NONE
            },
        }
    }

    fn act(request: PointerRequest, hit: Option<PointerHit>, f: &Facts) -> PointerAct {
        resolve(request, on(hit), Some(built(7)), f, kinds).act
    }

    #[test]
    fn a_row_moves_the_cursor_and_the_drawn_cursor_row_enters() {
        let f = facts();
        let l = press(Button::Left);
        assert_eq!(
            act(l, row(3, RowRefKind::Chart, Button::Left), &f),
            PointerAct::MoveTo(3)
        );
        assert_eq!(
            act(l, row(1, RowRefKind::Folder, Button::Left), &f),
            PointerAct::Key(MenuKey::Enter)
        );
        // The cursor moved since the build drew it on row 1.
        let moved = Facts { cursor: 2, ..f };
        assert_eq!(
            act(l, row(1, RowRefKind::Folder, Button::Left), &moved),
            PointerAct::MoveTo(1)
        );
        // Any other button on a row is Escape.
        let r = press(Button::Middle);
        assert_eq!(
            act(r, row(3, RowRefKind::Chart, Button::Middle), &f),
            PointerAct::Key(MenuKey::Escape)
        );
        // A stale list, an index past the list or another kind: nothing, in
        // every branch.
        let stale = Facts {
            list_revision: 3,
            ..f
        };
        for (hit, f) in [
            (row(3, RowRefKind::Chart, Button::Left), &stale),
            (row(1, RowRefKind::Folder, Button::Left), &stale),
            (row(3, RowRefKind::Chart, Button::Right), &stale),
            (row(5, RowRefKind::Chart, Button::Left), &f),
            (row(1, RowRefKind::Chart, Button::Left), &f),
            (row(1, RowRefKind::Chart, Button::Right), &f),
        ] {
            assert_eq!(act(l, hit, f), PointerAct::Nothing, "{hit:?}");
        }
    }

    #[test]
    fn an_input_of_another_lifetime_or_page_does_nothing() {
        let f = facts();
        let l = press(Button::Right);
        let hit = row(3, RowRefKind::Chart, Button::Right);
        // The positive control.
        assert_eq!(act(l, hit, &f), PointerAct::Key(MenuKey::Escape));
        // Observed under 7, built in 8; built in 7, dispatched in 8; the
        // word closed since; the built-in page.
        assert_eq!(resolve(l, on(hit), Some(built(8)), &f, kinds), NOTHING);
        let later = Facts {
            lifetime: 8,
            word: W8,
            ..f
        };
        assert_eq!(resolve(l, on(hit), Some(built(7)), &later, kinds), NOTHING);
        let closed = Facts { word: W7 & !1, ..f };
        assert_eq!(resolve(l, on(hit), Some(built(7)), &closed, kinds), NOTHING);
        let page = Built {
            skinned: false,
            ..built(7)
        };
        assert_eq!(resolve(l, on(None), Some(page), &f, kinds), NOTHING);
        assert_eq!(resolve(l, on(None), None, &f, kinds), NOTHING);
    }

    #[test]
    fn actions_sliders_and_empty_space() {
        let f = facts();
        let l = press(Button::Left);
        let action = |event, arg| Some(PointerHit::Action { event, arg });
        assert_eq!(act(l, action(12, 1), &f), PointerAct::Key(MenuKey::Right));
        assert_eq!(act(l, action(12, -1), &f), PointerAct::Key(MenuKey::Left));
        // 15 and 315 on a folder do nothing; on the drawn chart they act.
        assert_eq!(act(l, action(15, 1), &f), PointerAct::Nothing);
        let on_chart = Facts { cursor: 2, ..f };
        let chart_built = Built {
            cursor: Some(2),
            ..built(7)
        };
        assert_eq!(
            resolve(l, on(action(15, 1)), Some(chart_built), &on_chart, kinds).act,
            PointerAct::Key(MenuKey::Enter)
        );
        assert_eq!(
            resolve(l, on(action(315, 1)), Some(chart_built), &on_chart, kinds).act,
            PointerAct::Key(MenuKey::Practice)
        );
        assert_eq!(act(l, action(315, 1), &on_chart), PointerAct::Nothing);
        assert_eq!(act(l, action(340, 1), &f), PointerAct::Nothing);
        let slider = |source, n| {
            Some(PointerHit::Slider(SliderCommand {
                source,
                value: Ratio {
                    numerator: n,
                    denominator: 8,
                },
            }))
        };
        // Five rows: round(4 · 3/8) = round(1.5) = 2, ties up.
        assert_eq!(
            act(l, slider(RatioSource::ListPosition, 3), &f),
            PointerAct::MoveTo(2)
        );
        assert_eq!(
            act(l, slider(RatioSource::ListPosition, 8), &f),
            PointerAct::MoveTo(4)
        );
        assert_eq!(
            act(l, slider(RatioSource::MasterVolume, 8), &f),
            PointerAct::Nothing
        );
        let stale = Built {
            list_revision: 1,
            ..built(7)
        };
        assert_eq!(
            resolve(
                l,
                on(slider(RatioSource::ListPosition, 8)),
                Some(stale),
                &f,
                kinds
            )
            .act,
            PointerAct::Nothing
        );
        let one = Facts {
            count: 1,
            cursor: 0,
            ..f
        };
        assert_eq!(
            act(l, slider(RatioSource::ListPosition, 8), &one),
            PointerAct::Nothing
        );
        assert_eq!(
            act(press(Button::Right), None, &f),
            PointerAct::Key(MenuKey::Escape)
        );
        assert_eq!(act(press(Button::Middle), None, &f), PointerAct::Nothing);
    }

    #[test]
    fn the_search_box_the_ime_and_the_panels() {
        let f = facts();
        let l = press(Button::Right);
        // Any button in the box focuses it; a focused box keeps it.
        assert_eq!(
            resolve(l, on(SEARCH), Some(built(7)), &f, kinds),
            Resolved {
                unfocus: false,
                act: PointerAct::Key(MenuKey::Tab)
            }
        );
        let searching = Facts {
            searching: true,
            ..f
        };
        assert_eq!(
            resolve(l, on(SEARCH), Some(built(7)), &searching, kinds),
            NOTHING
        );
        // Outside it the focus goes first, then the press hits.
        let lp = press(Button::Left);
        assert_eq!(
            resolve(
                lp,
                on(row(3, RowRefKind::Chart, Button::Left)),
                Some(built(7)),
                &searching,
                kinds
            ),
            Resolved {
                unfocus: true,
                act: PointerAct::MoveTo(3)
            }
        );
        // While the IME composes, nothing.
        let composing = Facts {
            composing: true,
            ..searching
        };
        let wheel = PointerRequest::Wheel {
            word: W7,
            notches: 2,
        };
        for request in [lp, wheel] {
            assert_eq!(
                resolve(
                    request,
                    on(row(3, RowRefKind::Chart, Button::Left)),
                    Some(built(7)),
                    &composing,
                    kinds
                ),
                NOTHING
            );
        }
        // The wheel moves rows, search focus or not, and not under a panel.
        assert_eq!(act(wheel, None, &searching), PointerAct::Rows(2));
        let panel = Facts {
            panel: true,
            ..searching
        };
        assert_eq!(act(wheel, None, &panel), PointerAct::Nothing);
        // Under a panel only the order button acts, and focus stays.
        assert_eq!(
            resolve(
                lp,
                on(Some(PointerHit::Action { event: 12, arg: -1 })),
                Some(built(7)),
                &panel,
                kinds
            ),
            Resolved {
                unfocus: false,
                act: PointerAct::Key(MenuKey::Left)
            }
        );
        for hit in [row(3, RowRefKind::Chart, Button::Left), SEARCH, None] {
            assert_eq!(resolve(lp, on(hit), Some(built(7)), &panel, kinds), NOTHING);
            assert_eq!(
                resolve(press(Button::Right), on(hit), Some(built(7)), &panel, kinds),
                NOTHING
            );
        }
    }

    /// The focus goes by whether the press lay outside the box's bounds,
    /// not by what it hit (design.md "Input", **Pointer input**;
    /// `skin/Skin.java:394-397`).
    #[test]
    fn the_search_focus_goes_by_the_box_bounds_not_by_the_hit() {
        let searching = Facts {
            searching: true,
            ..facts()
        };
        let l = press(Button::Left);
        // The order button drawn over the box, inside its bounds: it acts
        // and the focus stays.
        let inside = PressHit {
            hit: Some(PointerHit::Action { event: 12, arg: 1 }),
            inside_search: SearchBoxes(1),
        };
        assert_eq!(
            resolve(l, inside, Some(built(7)), &searching, kinds),
            Resolved {
                unfocus: false,
                act: PointerAct::Key(MenuKey::Right)
            }
        );
        // A row outside the bounds: the focus goes, then the row acts.
        let outside = PressHit {
            hit: row(3, RowRefKind::Chart, Button::Left),
            inside_search: SearchBoxes::NONE,
        };
        assert_eq!(
            resolve(l, outside, Some(built(7)), &searching, kinds),
            Resolved {
                unfocus: true,
                act: PointerAct::MoveTo(3)
            }
        );
    }

    /// With two search boxes, the focus goes by the bounds of the box that
    /// took it: a press inside box 1 only drops box 0's focus, an action
    /// over it included, and a press inside box 0 only keeps it; a press on
    /// box 1 while box 0 has the focus moves the focus there (the source's
    /// `commitIfOutside`, then `focus`, `skin/Skin.java:394-404`). Focus the
    /// keyboard gave goes only outside every box.
    #[test]
    fn the_search_focus_goes_by_the_focused_box_s_bounds() {
        let l = press(Button::Left);
        let order = Some(PointerHit::Action { event: 12, arg: 1 });
        let at = |hit, inside| PressHit {
            hit,
            inside_search: SearchBoxes(inside),
        };
        let on_box = |box_: Option<u8>| Facts {
            searching: true,
            search_box: box_,
            ..facts()
        };
        let resolved = |p, f: &Facts| resolve(l, p, Some(built(7)), f, kinds);
        let unfocused = |act| Resolved { unfocus: true, act };
        let kept = |act| Resolved {
            unfocus: false,
            act,
        };
        let right = PointerAct::Key(MenuKey::Right);
        assert_eq!(
            resolved(at(order, 0b10), &on_box(Some(0))),
            unfocused(right)
        );
        assert_eq!(resolved(at(order, 0b01), &on_box(Some(0))), kept(right));
        assert_eq!(
            resolved(at(order, 0b01), &on_box(Some(1))),
            unfocused(right)
        );
        assert_eq!(resolved(at(order, 0b10), &on_box(Some(1))), kept(right));
        assert_eq!(
            resolved(
                at(Some(PointerHit::Search { index: 1 }), 0b10),
                &on_box(Some(0))
            ),
            unfocused(PointerAct::Key(MenuKey::Tab))
        );
        assert_eq!(
            resolved(
                at(Some(PointerHit::Search { index: 1 }), 0b10),
                &on_box(Some(1))
            ),
            NOTHING
        );
        assert_eq!(resolved(at(order, 0b10), &on_box(None)), kept(right));
        assert_eq!(resolved(at(order, 0), &on_box(None)), unfocused(right));
    }

    #[test]
    fn a_batch_is_answered_once_and_wakes_its_submitter() {
        let s = PointerShared::default();
        let mut out = [PointerSlot::EMPTY; BATCH];
        let mut requests = (0..70).map(|i| PointerRequest::Wheel {
            word: W7,
            notches: i,
        });
        let seq = s.submit(7, || requests.next()).unwrap();
        // One batch holds 64 at most; the rest stay with their source.
        assert_eq!(
            requests.next(),
            Some(PointerRequest::Wheel {
                word: W7,
                notches: 64
            })
        );
        assert_eq!(s.take_answers(seq, &mut out), Taken::Waiting);
        assert!(s.answer(Some(built(7)), |_| on(SEARCH)));
        assert!(!s.answer(Some(built(7)), |_| PressHit::NONE));
        assert_eq!(
            s.take_answers(seq, &mut out),
            Taken::Answered {
                built: Some(built(7)),
                len: 64
            }
        );
        assert_eq!(
            out[63].request,
            PointerRequest::Wheel {
                word: W7,
                notches: 63
            }
        );
        assert_eq!(out[0].hit, SEARCH);
        assert_eq!(s.take_answers(seq, &mut out), Taken::Gone);
        // A reset abandons a batch in flight.
        let seq = s.submit(7, || Some(press(Button::Left))).unwrap();
        s.reset();
        assert!(!s.answer(Some(built(7)), |_| PressHit::NONE));
        assert_eq!(s.take_answers(seq, &mut out), Taken::Gone);
        assert_eq!(s.submit(7, || None), None);
    }

    #[test]
    fn a_drag_waits_for_its_hold_and_the_latest_replaces_it() {
        let s = PointerShared::default();
        let cmd = |hold| DragCommand {
            word: W7,
            hold,
            built: built(7),
            command: SliderCommand {
                source: RatioSource::ListPosition,
                value: Ratio {
                    numerator: 1,
                    denominator: 2,
                },
            },
        };
        // Its press not yet taken out: it stays.
        s.put_drag(cmd(3));
        assert_eq!(s.take_drag(2), None);
        assert_eq!(s.take_drag(3), Some(cmd(3)));
        assert_eq!(s.take_drag(3), None);
        // A later press dispatched: dropped.
        s.put_drag(cmd(2));
        assert_eq!(s.take_drag(3), None);
        assert_eq!(s.take_drag(2), None);
        assert_eq!(resolve_drag(&cmd(2), &facts()), PointerAct::MoveTo(2));
        let panel = Facts {
            panel: true,
            ..facts()
        };
        assert_eq!(resolve_drag(&cmd(2), &panel), PointerAct::Nothing);
    }

    #[test]
    fn the_render_side_takes_one_move_a_frame_under_the_current_word() {
        let s = PointerShared::default();
        let (presses, queued) = (std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
        let mut c = Capture::default();
        s.publish(7);
        c.moved(&s, [1, 1], W7, 0);
        c.button(&s, Button::Left, true, W7, 0, admitting(&presses, &queued));
        let mut seen = Seen::default();
        assert_eq!(s.moved(&mut seen), None);
        c.moved(&s, [2, 1], W7, 0);
        c.moved(&s, [3, 1], W7, 0);
        assert_eq!(s.moved(&mut seen), Some((W7, 1, [3, 1])));
        assert_eq!(s.moved(&mut seen), None);
        // Moved, released and pressed again with no frame between: the
        // earlier hold's movement is not the new hold's.
        c.moved(&s, [4, 1], W7, 0);
        c.button(&s, Button::Left, false, W7, 0, |_| None);
        c.button(&s, Button::Left, true, W7, 0, admitting(&presses, &queued));
        assert_eq!(s.moved(&mut seen), None);
        c.moved(&s, [5, 1], W7, 0);
        assert_eq!(s.moved(&mut seen), Some((W7, 2, [5, 1])));
        c.moved(&s, [6, 1], W7, 0);
        s.close();
        assert_eq!(s.moved(&mut seen), None);
    }

    /// P15, P16: a hold of lifetime 7 moved and left unseen (no frame
    /// between), then selection entered again as 8 and pressed afresh with
    /// no move: the render side finds no drag of the lifetime-8 hold, and
    /// the hold's own move gives one.
    #[test]
    fn a_hold_of_one_lifetime_lends_no_movement_to_the_next() {
        let s = PointerShared::default();
        let (presses, queued) = (std::cell::Cell::new(0), std::cell::RefCell::new(Vec::new()));
        let mut c = Capture::default();
        let mut seen = Seen::default();
        s.publish(7);
        c.moved(&s, [1, 1], W7, 0);
        c.button(&s, Button::Left, true, W7, 0, admitting(&presses, &queued));
        assert_eq!(s.moved(&mut seen), None);
        c.moved(&s, [9, 1], W7, 0);
        s.publish(8);
        c.button(&s, Button::Left, true, W8, 1, admitting(&presses, &queued));
        assert_eq!(s.drag_now().word, W8);
        assert_eq!(s.moved(&mut seen), None);
        c.moved(&s, [5, 1], W8, 1);
        assert_eq!(s.moved(&mut seen), Some((W8, 2, [5, 1])));
    }

    #[test]
    fn a_slot_fits_its_bound() {
        assert!(
            size_of::<PointerSlot>() <= 64,
            "{}",
            size_of::<PointerSlot>()
        );
        assert!(
            size_of::<DragCommand>() <= 96,
            "{}",
            size_of::<DragCommand>()
        );
    }
}
