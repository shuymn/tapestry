//! The player's own sounds on the audio thread (design.md "Audio", system
//! sounds): a pool of system voices, the generations they play from, and the
//! decision tone.
//!
//! A generation ([`SystemGeneration`]) is one pick of the sets, decoded off
//! the audio thread. The requesting thread numbers every start, stop and
//! install from one sequence and hands them over through preallocated rings
//! and words; [`SystemVoices::apply`] takes them at the start of each audio
//! step from a [`SystemSnapshot`] of those rings and words, in sequence order
//! up to the snapshot's cutoff, so a start never overtakes an install
//! numbered below it. A replaced generation is retired while a voice still
//! plays from it and goes back through the return ring once it is silent.
//! A generation leaves its place only into a return slot reserved first: an
//! install or a sweep that finds no room waits for a later step, so every
//! generation stays owned. Nothing here allocates or frees after
//! [`SystemVoices::new`].
//!
//! System voices and the tone are summed onto the chart's output at the
//! system volume and the sum is clamped once ([`SystemVoices::mix_into`]);
//! no chart operation touches them.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use rtrb::chunks::WriteChunkUninit;
use rtrb::{Consumer, Producer};

use crate::CHANNELS;
use crate::bank::Sound;

/// System voices: a start that finds them all sounding is refused and
/// counted, never stealing one. Provisional: 32 voices hold a scroll sound
/// started every 50 ms for 1.6 s.
pub const SYSTEM_VOICES: usize = 32;
/// Entries of a producer's start ring. Provisional.
pub const SYSTEM_STARTS: usize = 64;
/// Entries of the install ring: one pick's install and the empty install of
/// tone-only recovery.
pub const SYSTEM_INSTALLS: usize = 2;
/// Entries of the return ring: the retired generation and the current one
/// at an empty install, plus the one an install held behind a taken
/// retired slot adds. The requesting side hands over at most this many
/// generations it has not taken back, and one more for the empty install:
/// the current one never waits in the ring, so the ring always has room
/// for the rest.
pub const SYSTEM_RETURNS: usize = 3;
/// The kinds a generation can hold.
pub const SYSTEM_KINDS: usize = 13;

/// Whether a start numbered `seq` of `kind` falls below its kind's stop.
fn stopped(stops: &[u64; SYSTEM_KINDS], kind: SystemKind, seq: u64) -> bool {
    stops.get(kind.index()).is_some_and(|&word| seq < word)
}

/// A kind of system sound: the source's sound files of selection, the
/// decision, the ready and closing sounds, and the result's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SystemKind {
    /// Selection's BGM (a loop).
    Select,
    /// The decision sound; the synthesized tone when a generation has none.
    Decide,
    /// Selection's scroll.
    Scratch,
    /// A folder opened.
    FolderOpen,
    /// A folder closed.
    FolderClose,
    /// The options opened.
    OptionOpen,
    /// The options closed.
    OptionClose,
    /// An option changed.
    OptionChange,
    /// The play is ready.
    PlayReady,
    /// The play stopped.
    PlayStop,
    /// The result of a cleared play (a loop).
    Clear,
    /// The result of a failed play (a loop).
    Fail,
    /// The result closed.
    ResultClose,
}

impl SystemKind {
    /// Every kind, in [`index`](Self::index) order.
    pub const ALL: [Self; SYSTEM_KINDS] = [
        Self::Select,
        Self::Decide,
        Self::Scratch,
        Self::FolderOpen,
        Self::FolderClose,
        Self::OptionOpen,
        Self::OptionClose,
        Self::OptionChange,
        Self::PlayReady,
        Self::PlayStop,
        Self::Clear,
        Self::Fail,
        Self::ResultClose,
    ];

    /// Its place in [`ALL`](Self::ALL).
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// A generation's id: drawn from the request sequence, so a later
/// generation has a larger id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenId(pub u64);

#[cfg(debug_assertions)]
thread_local! {
    /// Whether this thread is inside a system step.
    static IN_STEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks this thread inside a system step while it lives (debug builds):
/// a generation dropped meanwhile is freed on the audio thread.
struct Stepping {
    #[cfg(debug_assertions)]
    was: bool,
}

impl Stepping {
    fn enter() -> Self {
        Self {
            #[cfg(debug_assertions)]
            was: IN_STEP.replace(true),
        }
    }
}

impl Drop for Stepping {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        IN_STEP.set(self.was);
    }
}

/// One pick of the sets: each distinct sound once, the kinds that play
/// it, and the bytes it holds. The audio thread only reads it and moves
/// it; it is built and dropped elsewhere, never in a step (debug builds
/// assert it).
#[derive(Debug)]
pub struct SystemGeneration {
    id: GenId,
    rate: u32,
    sounds: Vec<Arc<Sound>>,
    /// Per kind, its sound's index in `sounds`.
    kinds: [Option<u8>; SYSTEM_KINDS],
    bytes: u64,
    /// The empty generation of tone-only recovery: its install stops
    /// every set sound.
    empty: bool,
}

impl SystemGeneration {
    /// A generation at `rate` with no sound yet ([`with`](Self::with)).
    #[must_use]
    pub fn new(id: GenId, rate: u32) -> Self {
        Self {
            id,
            rate,
            sounds: Vec::with_capacity(SYSTEM_KINDS),
            kinds: [None; SYSTEM_KINDS],
            bytes: 0,
            empty: false,
        }
    }

    /// The empty generation of tone-only recovery: no sound, and its
    /// install is a stop of every kind at its number, so no set sound plays
    /// after it and the decision sound is the tone.
    #[must_use]
    pub fn empty(id: GenId) -> Self {
        Self {
            id,
            rate: 0,
            sounds: Vec::new(),
            kinds: [None; SYSTEM_KINDS],
            bytes: 0,
            empty: true,
        }
    }

    /// `kind` plays `sound`, stored once however many kinds share it (the
    /// same `Arc`) and counted once in [`bytes`](Self::bytes). The empty
    /// generation takes none.
    #[must_use]
    pub fn with(mut self, kind: SystemKind, sound: Arc<Sound>) -> Self {
        if self.empty {
            return self;
        }
        let at = match self.sounds.iter().position(|s| Arc::ptr_eq(s, &sound)) {
            Some(at) => at,
            None => {
                self.bytes += sound.bytes();
                self.sounds.push(sound);
                self.sounds.len() - 1
            }
        };
        if let (Some(slot), Ok(at)) = (self.kinds.get_mut(kind.index()), u8::try_from(at)) {
            *slot = Some(at);
        }
        self
    }

    /// Its id.
    #[must_use]
    pub const fn id(&self) -> GenId {
        self.id
    }

    /// The rate its sounds were decoded at (0 for the empty generation).
    #[must_use]
    pub const fn rate(&self) -> u32 {
        self.rate
    }

    /// The decoded bytes of its sounds, each counted once.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether it is the empty generation of tone-only recovery.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.empty
    }

    /// The sound `kind` plays, if it has one.
    #[must_use]
    pub fn sound(&self, kind: SystemKind) -> Option<&Arc<Sound>> {
        self.sounds.get(usize::from(self.index(kind)?))
    }

    fn index(&self, kind: SystemKind) -> Option<u8> {
        self.kinds.get(kind.index()).copied().flatten()
    }

    fn stored(&self, at: u8) -> Option<&Sound> {
        self.sounds.get(usize::from(at)).map(|s| &**s)
    }
}

impl Drop for SystemGeneration {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        debug_assert!(
            !IN_STEP.get() || std::thread::panicking(),
            "a system generation freed during an audio step"
        );
    }
}

/// A start request: `kind` from the generation current at its number, or,
/// bound to generation `generation` (a loop), only from that one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemStart {
    /// The kind.
    pub kind: SystemKind,
    /// Whether it loops until stopped.
    pub looped: bool,
    /// The generation it is bound to: it plays when that one is current,
    /// waits for it while it is later, and is discarded once it is earlier.
    pub generation: Option<GenId>,
    /// Its number in the request sequence.
    pub seq: u64,
}

/// An install request: `generation` becomes current at number `seq`.
#[derive(Debug)]
pub struct SystemInstall {
    /// The generation.
    pub generation: SystemGeneration,
    /// Its number in the request sequence.
    pub seq: u64,
}

/// What one step takes ([`SystemVoices::apply`]), read before either ring:
/// the requests numbered up to `cutoff` are all in their rings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemSnapshot {
    /// The decision tone was asked for.
    pub tone: bool,
    /// The highest number the requesting thread has published.
    pub cutoff: u64,
    /// Starts readable in the start ring.
    pub starts: usize,
    /// Installs readable in the install ring.
    pub installs: usize,
    /// Per kind ([`SystemKind::index`]), its stop: every voice of the kind
    /// started below it ends, and a start below it is discarded.
    pub stops: [u64; SYSTEM_KINDS],
}

/// What one [`SystemVoices::apply`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// The last generation it installed.
    pub installed: Option<GenId>,
    /// Starts it took from the ring.
    pub taken: u32,
    /// Whether its merge ended at an install that found the retired slot
    /// taken, or no room in the return ring for what it sends back: that
    /// install and every request above it stay queued.
    pub blocked: bool,
    /// Whether it started the tone.
    pub tone: bool,
}

/// A system voice: one sound of a generation, from its first frame.
#[derive(Clone, Copy, Debug)]
struct Voice {
    active: bool,
    kind: SystemKind,
    generation: GenId,
    /// The sound's index in its generation.
    sound: u8,
    /// The next frame.
    pos: u64,
    looped: bool,
    seq: u64,
}

impl Voice {
    const IDLE: Self = Self {
        active: false,
        kind: SystemKind::Select,
        generation: GenId(0),
        sound: 0,
        pos: 0,
        looped: false,
        seq: 0,
    };
}

/// The decision tone's partials: frequency in Hz and weight.
const TONE_PARTIALS: [(f32, f32); 2] = [(880.0, 0.6), (1_320.0, 0.4)];
/// The decision tone's length, its attack and its level before the system
/// volume.
const TONE_SECONDS: f32 = 0.6;
const TONE_ATTACK_SECONDS: f32 = 0.005;
const TONE_LEVEL: f32 = 0.25;

/// The decision tone (design.md "Play Lifecycle", the decision screen's
/// sound): a short chime synthesized in `f32`, outside every bank and
/// generation, one instance; a new start cuts the sounding one, and no set
/// stop, install or chart operation touches it. It allocates nothing.
#[derive(Debug)]
struct Tone {
    rate: f32,
    /// Its length in frames.
    length: u32,
    /// The next frame of the sounding tone; `None` when silent.
    at: Option<u32>,
}

impl Tone {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a rate up to MAX_RATE and a length under a second fit an f32 and a u32"
    )]
    fn new(rate: u32) -> Self {
        let rate = rate as f32;
        Self {
            rate,
            length: (TONE_SECONDS * rate) as u32,
            at: None,
        }
    }

    /// Starts it from its beginning, sounding or not.
    const fn start(&mut self) {
        self.at = Some(0);
    }

    const fn sounding(&self) -> bool {
        self.at.is_some()
    }

    /// Its sample at frame `at`.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a frame under a second of samples is exact in an f32"
    )]
    fn sample(&self, at: u32) -> f32 {
        let t = at as f32 / self.rate;
        let attack = (t / TONE_ATTACK_SECONDS).min(1.0);
        let fall = 1.0 - t / TONE_SECONDS;
        let envelope = attack * fall * fall;
        let sum: f32 = TONE_PARTIALS
            .iter()
            .map(|&(f, w)| w * (std::f32::consts::TAU * f * t).sin())
            .sum();
        sum * envelope * TONE_LEVEL
    }

    /// Its next sample at `volume`, or `None` once it has ended.
    fn next(&mut self, volume: f32) -> Option<f32> {
        let at = self.at?;
        if at >= self.length {
            self.at = None;
            return None;
        }
        self.at = Some(at + 1);
        Some(self.sample(at) * volume)
    }
}

/// The system voices, the generations they play from and the decision
/// tone, owned by the audio thread. Built with the engine; nothing after
/// [`new`](Self::new) allocates or frees.
#[derive(Debug)]
pub struct SystemVoices {
    voices: [Voice; SYSTEM_VOICES],
    current: Option<SystemGeneration>,
    /// The replaced generation a voice still plays from.
    retired: Option<SystemGeneration>,
    /// A loop waiting for its later generation.
    pending: Option<SystemStart>,
    tone: Tone,
    /// A tone request taken in a step whose merge ended at a blocked
    /// install: it starts once a later step's merge reaches its cutoff.
    tone_held: bool,
    /// The output failed: no voice sounds again.
    failed: bool,
    /// Starts refused: a full pool, or after the failure.
    refused: u64,
    /// Steps whose merge ended at an install the taken retired slot or a
    /// full return ring held.
    install_waits: u64,
}

/// What an install sends back: the old current unless it retires, and, at
/// an empty install, the retired one too.
#[derive(Clone, Copy, Debug)]
struct SendsBack {
    current: bool,
    retired: bool,
}

impl SendsBack {
    /// The return slots it takes.
    fn slots(self) -> usize {
        usize::from(self.current) + usize::from(self.retired)
    }
}

impl SystemVoices {
    /// Silent voices for an output at `rate`, no generation installed.
    #[must_use]
    pub fn new(rate: u32) -> Self {
        Self {
            voices: [Voice::IDLE; SYSTEM_VOICES],
            current: None,
            retired: None,
            pending: None,
            tone: Tone::new(rate),
            tone_held: false,
            failed: false,
            refused: 0,
            install_waits: 0,
        }
    }

    /// Voices sounding.
    #[must_use]
    pub fn sounding(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    /// The current generation's id.
    #[must_use]
    pub fn current(&self) -> Option<GenId> {
        self.current.as_ref().map(SystemGeneration::id)
    }

    /// The retired generation's id.
    #[must_use]
    pub fn retired(&self) -> Option<GenId> {
        self.retired.as_ref().map(SystemGeneration::id)
    }

    /// The loop waiting for its generation.
    #[must_use]
    pub const fn pending(&self) -> Option<SystemStart> {
        self.pending
    }

    /// Whether the decision tone sounds.
    #[must_use]
    pub const fn tone_sounding(&self) -> bool {
        self.tone.sounding()
    }

    /// Whether a tone request waits behind a blocked install.
    #[must_use]
    pub const fn tone_held(&self) -> bool {
        self.tone_held
    }

    /// Starts refused so far: a full pool, or any after the output failed.
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.refused
    }

    /// Steps so far whose merge ended at an install the taken retired slot
    /// or a full return ring held.
    #[must_use]
    pub const fn install_waits(&self) -> u64 {
        self.install_waits
    }

    /// Starts the decision tone from its beginning at the next block, as a
    /// taken tone request does; nothing after the output failed.
    pub const fn start_tone(&mut self) {
        if !self.failed {
            self.tone.start();
        }
    }

    /// One step's requests (the control step): every empty install the
    /// merge will take raises every kind's stop to its number; the stops
    /// end the voices and the waiting loop below them, in every
    /// generation; then the starts and installs numbered up to the cutoff
    /// apply in number order, each start against the generation current
    /// at its place, until an install that finds the retired slot taken,
    /// or too few free slots in `returns` for what it sends back, which
    /// stays queued with everything above it. A tone request starts the
    /// tone after the step's installs, or waits, held, behind a blocked
    /// install. A replaced generation retires while a voice plays from it
    /// and otherwise goes to `returns` at once.
    pub fn apply(
        &mut self,
        snap: &SystemSnapshot,
        starts: &mut Consumer<SystemStart>,
        installs: &mut Consumer<SystemInstall>,
        returns: &mut Producer<SystemGeneration>,
    ) -> ApplyReport {
        let _stepping = Stepping::enter();
        let mut report = ApplyReport::default();
        let mut stops = snap.stops;
        let installs_left = snap.installs.min(installs.slots());
        let held_at = self.scan_installs(snap.cutoff, installs, installs_left, &mut stops);
        self.stop_below(&stops);
        report.blocked = self.merge(
            snap,
            &stops,
            held_at,
            starts,
            installs,
            returns,
            &mut report,
        );
        if report.blocked {
            self.install_waits += 1;
        }
        let asked = snap.tone || self.tone_held;
        if self.failed {
            if asked {
                self.refused += 1;
            }
            self.tone_held = false;
        } else if asked {
            if report.blocked {
                self.tone_held = true;
            } else {
                self.tone.start();
                self.tone_held = false;
                report.tone = true;
            }
        }
        report
    }

    /// The installs up to `cutoff`, in order, without taking them: each
    /// empty one raises every kind's stop to its number. Returns the
    /// number of the first that will find the retired slot taken, where
    /// the merge ends: one while a generation is retired, or a second
    /// pick's install after one that may retire the current.
    fn scan_installs(
        &self,
        cutoff: u64,
        installs: &mut Consumer<SystemInstall>,
        left: usize,
        stops: &mut [u64; SYSTEM_KINDS],
    ) -> Option<u64> {
        let Ok(chunk) = installs.read_chunk(left) else {
            return None;
        };
        let (a, b) = chunk.as_slices();
        let mut retired = self.retired.is_some();
        let mut current = self.current.is_some();
        for install in a.iter().chain(b) {
            if install.seq > cutoff {
                break;
            }
            if install.generation.is_empty() {
                for word in stops.iter_mut() {
                    *word = (*word).max(install.seq);
                }
                retired = false;
            } else if retired {
                return Some(install.seq);
            } else {
                retired = current;
            }
            current = true;
        }
        // Dropped uncommitted: every install stays in the ring.
        None
    }

    /// Ends every voice, and discards the waiting loop, started below its
    /// kind's stop.
    fn stop_below(&mut self, stops: &[u64; SYSTEM_KINDS]) {
        for v in self.voices.iter_mut().filter(|v| v.active) {
            if stopped(stops, v.kind, v.seq) {
                v.active = false;
            }
        }
        if self.pending.is_some_and(|p| stopped(stops, p.kind, p.seq)) {
            self.pending = None;
        }
    }

    /// The starts and installs up to the cutoff, merged by number; `true`
    /// when it ended at a blocked install.
    #[expect(
        clippy::too_many_arguments,
        reason = "the snapshot, its raised stops and the three rings are one step's inputs"
    )]
    fn merge(
        &mut self,
        snap: &SystemSnapshot,
        stops: &[u64; SYSTEM_KINDS],
        held_at: Option<u64>,
        starts: &mut Consumer<SystemStart>,
        installs: &mut Consumer<SystemInstall>,
        returns: &mut Producer<SystemGeneration>,
        report: &mut ApplyReport,
    ) -> bool {
        let mut starts_left = snap.starts.min(starts.slots());
        let mut installs_left = snap.installs.min(installs.slots());
        loop {
            let start = starts
                .peek()
                .ok()
                .copied()
                .filter(|s| starts_left > 0 && s.seq <= snap.cutoff);
            let install = installs
                .peek()
                .ok()
                .map(|i| (i.seq, i.generation.is_empty()))
                .filter(|&(seq, _)| installs_left > 0 && seq <= snap.cutoff);
            match (start, install) {
                (Some(s), install) if install.is_none_or(|(seq, _)| s.seq < seq) => {
                    let _ = starts.pop();
                    starts_left -= 1;
                    report.taken += 1;
                    self.resolve(s, stops);
                }
                (_, Some((seq, empty))) => {
                    if held_at == Some(seq) || (!empty && self.retired.is_some()) {
                        return true;
                    }
                    let back = self.sends_back(empty);
                    let Ok(room) = returns.write_chunk_uninit(back.slots()) else {
                        return true;
                    };
                    let Ok(install) = installs.pop() else {
                        return false;
                    };
                    installs_left -= 1;
                    report.installed = Some(install.generation.id());
                    self.install(install.generation, back, room);
                }
                // Nothing left up to the cutoff.
                _ => return false,
            }
        }
    }

    /// One start, against the generation current at its place.
    fn resolve(&mut self, s: SystemStart, stops: &[u64; SYSTEM_KINDS]) {
        if self.failed {
            self.refused += 1;
            return;
        }
        if stopped(stops, s.kind, s.seq) {
            return;
        }
        let current = self.current();
        match s.generation {
            None => self.play(s),
            Some(g) if Some(g) == current => self.play(s),
            Some(g) if current.is_none_or(|c| g > c) => self.pending = Some(s),
            // Bound to an earlier generation: discarded.
            Some(_) => {}
        }
    }

    /// Starts `s` from the current generation: the decision sound falls
    /// back to the tone when it has none, any other kind is silent.
    fn play(&mut self, s: SystemStart) {
        let found = self
            .current
            .as_ref()
            .and_then(|g| Some((g.id(), g.index(s.kind)?)));
        let Some((generation, sound)) = found else {
            if s.kind == SystemKind::Decide {
                self.tone.start();
            }
            return;
        };
        let Some(v) = self.voices.iter_mut().find(|v| !v.active) else {
            self.refused += 1;
            return;
        };
        *v = Voice {
            active: true,
            kind: s.kind,
            generation,
            sound,
            pos: 0,
            looped: s.looped,
            seq: s.seq,
        };
    }

    fn has_voice(&self, id: GenId) -> bool {
        self.voices.iter().any(|v| v.active && v.generation == id)
    }

    /// What an install, empty or not, would send back now: the empty
    /// generation sends the old current and the retired one; otherwise the
    /// old current retires while a voice plays from it and the slot is
    /// free.
    fn sends_back(&self, empty: bool) -> SendsBack {
        let current = self
            .current
            .as_ref()
            .is_some_and(|old| empty || !(self.retired.is_none() && self.has_voice(old.id())));
        SendsBack {
            current,
            retired: empty && self.retired.is_some(),
        }
    }

    /// `generation` becomes current, and what `back` names goes into
    /// `room`, reserved for it. A loop waiting for `generation` starts; one
    /// for an earlier generation is discarded.
    fn install(
        &mut self,
        generation: SystemGeneration,
        back: SendsBack,
        room: WriteChunkUninit<'_, SystemGeneration>,
    ) {
        let id = generation.id();
        if generation.is_empty() {
            // Its stop ended every set voice already; none plays on.
            for v in &mut self.voices {
                v.active = false;
            }
        }
        let old = self.current.replace(generation);
        let (old_back, old_kept) = if back.current {
            (old, None)
        } else {
            (None, old)
        };
        let retired_back = if back.retired {
            self.retired.take()
        } else {
            None
        };
        if old_kept.is_some() {
            self.retired = old_kept;
        }
        room.fill_from_iter(old_back.into_iter().chain(retired_back));
        match self.pending {
            Some(p) if p.generation == Some(id) => {
                self.pending = None;
                self.play(p);
            }
            Some(p) if p.generation.is_some_and(|g| g < id) => self.pending = None,
            _ => {}
        }
    }

    /// Sends a retired generation that no voice plays from any more back
    /// through `returns`: after the mix, and at each control step after
    /// the output failed. With no free slot it stays retired until a later
    /// sweep finds one.
    pub fn sweep(&mut self, returns: &mut Producer<SystemGeneration>) {
        let _stepping = Stepping::enter();
        if let Some(id) = self.retired()
            && !self.has_voice(id)
            && let Ok(room) = returns.write_chunk_uninit(1)
        {
            room.fill_from_iter(self.retired.take());
        }
    }

    /// The output failed: every voice and the tone end, and the waiting
    /// loop is discarded (counted); from now on every start and tone
    /// request is discarded and counted, while stops, installs and the
    /// sweep still apply.
    pub fn fail(&mut self) {
        for v in &mut self.voices {
            v.active = false;
        }
        if self.pending.take().is_some() {
            self.refused += 1;
        }
        self.tone.at = None;
        self.tone_held = false;
        self.failed = true;
    }

    /// Adds the voices and the tone at `volume` to the interleaved block
    /// `buf` (the chart's output) and clamps the sum once, to full scale;
    /// leaves `buf` untouched when nothing sounds.
    pub fn mix_into(&mut self, buf: &mut [f32], volume: f32) {
        let _stepping = Stepping::enter();
        if !self.tone.sounding() && !self.voices.iter().any(|v| v.active) {
            return;
        }
        let Self {
            voices,
            current,
            retired,
            ..
        } = self;
        for v in voices.iter_mut().filter(|v| v.active) {
            let sound = [current.as_ref(), retired.as_ref()]
                .into_iter()
                .flatten()
                .find(|g| g.id() == v.generation)
                .and_then(|g| g.stored(v.sound));
            let Some(sound) = sound.filter(|s| s.frames() > 0) else {
                v.active = false;
                continue;
            };
            let frames = sound.frames();
            for frame in buf.as_chunks_mut::<CHANNELS>().0 {
                if v.pos >= frames {
                    if !v.looped {
                        break;
                    }
                    v.pos = 0;
                }
                let at = usize::try_from(v.pos).unwrap_or(usize::MAX);
                let (l, r) = sound.frame(at);
                for (x, s) in frame.iter_mut().zip([l, r]) {
                    *x += s * volume;
                }
                v.pos += 1;
            }
            if !v.looped && v.pos >= frames {
                v.active = false;
            }
        }
        for frame in buf.as_chunks_mut::<CHANNELS>().0 {
            let tone = self.tone.next(volume);
            for x in frame {
                *x = tone.map_or(*x, |s| *x + s).clamp(-1.0, 1.0);
            }
        }
    }
}
