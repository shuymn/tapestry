//! A requesting side for [`SystemVoices`] driven by hand: the three rings,
//! the stop words and the tone word, each step's snapshot taken with an
//! explicit cutoff.

#![allow(dead_code)]

use std::sync::Arc;

use audio::{
    ApplyReport, GenId, SYSTEM_INSTALLS, SYSTEM_KINDS, SYSTEM_RETURNS, SYSTEM_STARTS, Sound,
    SystemGeneration, SystemInstall, SystemKind, SystemSnapshot, SystemStart, SystemVoices,
};
use rtrb::{Consumer, Producer, RingBuffer};

pub const RATE: u32 = 48_000;

/// A sound of `frames` mono frames, each `level`.
pub fn constant(level: i16, frames: usize) -> Arc<Sound> {
    Arc::new(Sound::mono(vec![level; frames]))
}

/// The rings and words between a requesting thread and the voices.
pub struct Rig {
    pub starts_tx: Producer<SystemStart>,
    pub starts_rx: Consumer<SystemStart>,
    pub installs_tx: Producer<SystemInstall>,
    pub installs_rx: Consumer<SystemInstall>,
    pub returns_tx: Producer<SystemGeneration>,
    pub returns_rx: Consumer<SystemGeneration>,
    pub stops: [u64; SYSTEM_KINDS],
    pub tone: bool,
}

impl Rig {
    pub fn new() -> Self {
        let (starts_tx, starts_rx) = RingBuffer::new(SYSTEM_STARTS);
        let (installs_tx, installs_rx) = RingBuffer::new(SYSTEM_INSTALLS);
        let (returns_tx, returns_rx) = RingBuffer::new(SYSTEM_RETURNS);
        Self {
            starts_tx,
            starts_rx,
            installs_tx,
            installs_rx,
            returns_tx,
            returns_rx,
            stops: [0; SYSTEM_KINDS],
            tone: false,
        }
    }

    /// A one-shot of `kind` numbered `seq`.
    pub fn start(&mut self, seq: u64, kind: SystemKind) {
        self.push(SystemStart {
            kind,
            looped: false,
            generation: None,
            seq,
        });
    }

    /// A loop of `kind` numbered `seq`, bound to `generation`.
    pub fn looped(&mut self, seq: u64, kind: SystemKind, generation: GenId) {
        self.push(SystemStart {
            kind,
            looped: true,
            generation: Some(generation),
            seq,
        });
    }

    pub fn push(&mut self, start: SystemStart) {
        self.starts_tx.push(start).unwrap();
    }

    /// `generation` installed at `seq`.
    pub fn install(&mut self, seq: u64, generation: SystemGeneration) {
        self.installs_tx
            .push(SystemInstall { generation, seq })
            .unwrap();
    }

    /// Raises `kind`'s stop to `seq`.
    pub fn stop(&mut self, seq: u64, kind: SystemKind) {
        let word = &mut self.stops[kind.index()];
        *word = (*word).max(seq);
    }

    /// The snapshot a step takes with cutoff `cutoff`; the tone word is
    /// taken.
    pub fn snapshot(&mut self, cutoff: u64) -> SystemSnapshot {
        SystemSnapshot {
            tone: std::mem::take(&mut self.tone),
            cutoff,
            starts: self.starts_rx.slots(),
            installs: self.installs_rx.slots(),
            stops: self.stops,
        }
    }

    /// One control step through `cutoff`.
    pub fn apply(&mut self, voices: &mut SystemVoices, cutoff: u64) -> ApplyReport {
        let snap = self.snapshot(cutoff);
        voices.apply(
            &snap,
            &mut self.starts_rx,
            &mut self.installs_rx,
            &mut self.returns_tx,
        )
    }

    pub fn sweep(&mut self, voices: &mut SystemVoices) {
        voices.sweep(&mut self.returns_tx);
    }

    /// The ids of the generations returned so far, taken.
    pub fn returned(&mut self) -> Vec<GenId> {
        let mut ids = Vec::new();
        while let Ok(g) = self.returns_rx.pop() {
            ids.push(g.id());
        }
        ids
    }
}

/// `frames` frames of the voices over silence at `volume`.
pub fn mix(voices: &mut SystemVoices, frames: usize, volume: f32) -> Vec<f32> {
    let mut buf = vec![0.0; 2 * frames];
    voices.mix_into(&mut buf, volume);
    buf
}
