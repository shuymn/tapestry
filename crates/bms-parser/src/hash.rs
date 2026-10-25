//! File hashes and the resolved-chart hash.

use bms_model::{Chart, JudgeRank, LnMode, NoteKind, ScrollChange, SoundId};
use md5::Md5;
use sha2::{Digest, Sha256};

/// SHA-256 and MD5 of the file bytes.
pub fn file_hashes(bytes: &[u8]) -> ([u8; 32], [u8; 16]) {
    (Sha256::digest(bytes).into(), Md5::digest(bytes).into())
}

/// SHA-256 over a canonical encoding of the resolved chart: loader version,
/// scroll changes, notes, BGM, bar lines, sound names as written, header
/// values and `#RANDOM` results. Resolved sound paths are left out, so moving
/// the chart folder keeps the hash.
pub fn resolved_hash(chart: &Chart, scroll: &[ScrollChange]) -> [u8; 32] {
    let mut h = Encoder(Sha256::new());
    h.bytes(b"tapestry resolved chart");
    h.u64(u64::from(chart.identity.loader_version));
    h.i64(chart.scroll.initial_bpm_micro());
    h.len(scroll.len());
    for change in scroll {
        match *change {
            ScrollChange::Bpm { at, bpm_micro } => {
                h.u8(0);
                h.i64(at.0);
                h.i64(bpm_micro);
            }
            ScrollChange::Stop { at, len } => {
                h.u8(1);
                h.i64(at.0);
                h.i64(len.0);
            }
        }
    }
    for lane in &chart.lanes {
        h.len(lane.len());
        for note in lane {
            h.i64(note.time.0);
            h.sound(note.sound);
            match note.kind {
                NoteKind::Tap => h.u8(0),
                NoteKind::Long { end, end_sound } => {
                    h.u8(1);
                    h.i64(end.0);
                    h.sound(end_sound);
                }
                NoteKind::Mine { damage } => {
                    h.u8(2);
                    h.i64(i64::from(damage));
                }
            }
        }
    }
    h.len(chart.bgm.len());
    for event in &chart.bgm {
        h.i64(event.time.0);
        h.sound(Some(event.sound));
    }
    h.len(chart.bar_lines.len());
    for bar in &chart.bar_lines {
        h.i64(bar.0);
    }
    h.len(chart.sounds.len());
    for sound in &chart.sounds {
        h.bytes(sound.name.as_bytes());
    }
    h.bytes(chart.meta.title.as_bytes());
    match chart.meta.rank {
        None => h.u8(0),
        Some(JudgeRank::Rank(r)) => {
            h.u8(1);
            h.i64(i64::from(r));
        }
        Some(JudgeRank::DefExRankMilli(r)) => {
            h.u8(2);
            h.i64(r);
        }
    }
    match chart.meta.total_milli {
        None => h.u8(0),
        Some(t) => {
            h.u8(1);
            h.i64(t);
        }
    }
    h.u8(match chart.meta.ln_mode {
        None => 0,
        Some(LnMode::Ln) => 1,
        Some(LnMode::Cn) => 2,
        Some(LnMode::Hcn) => 3,
    });
    h.len(chart.identity.random_results.len());
    for &r in &chart.identity.random_results {
        h.u64(u64::from(r));
    }
    h.0.finalize().into()
}

/// Fixed-width little-endian fields; byte strings carry their length.
struct Encoder(Sha256);

impl Encoder {
    fn u8(&mut self, v: u8) {
        self.0.update([v]);
    }

    fn u64(&mut self, v: u64) {
        self.0.update(v.to_le_bytes());
    }

    fn i64(&mut self, v: i64) {
        self.0.update(v.to_le_bytes());
    }

    fn len(&mut self, n: usize) {
        self.u64(n as u64);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.update(b);
    }

    /// A sound by its index; the names are encoded with the sound table.
    fn sound(&mut self, s: Option<SoundId>) {
        match s {
            None => self.u8(0),
            Some(SoundId(id)) => {
                self.u8(1);
                self.u64(u64::from(id));
            }
        }
    }
}
