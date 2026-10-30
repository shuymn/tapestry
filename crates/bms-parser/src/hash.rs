//! File hashes, the resolved-chart hash and the played-chart hash.

use bms_model::{Chart, HeaderValue, JudgmentHeaderName, LnMode, NoteKind, SoundId, Speed};
use md5::Md5;
use sha2::{Digest, Sha256};

/// SHA-256 and MD5 of the file bytes.
pub fn file_hashes(bytes: &[u8]) -> ([u8; 32], [u8; 16]) {
    (Sha256::digest(bytes).into(), Md5::digest(bytes).into())
}

/// SHA-256 over a canonical encoding of the resolved 1× chart (the loader
/// hashes it before mapping times to a speed): loader version,
/// the scroll map (initial rate and segments), notes with their display
/// positions, BGM, bar-line positions, sound names as written, the title,
/// the judgment-header declarations in their canonical form (header, class
/// and exact value; never the text) and the LN mode. Resolved sound paths
/// are left out, so moving the chart folder
/// keeps the hash, and so are the `#RANDOM` results, which identify how the
/// chart was resolved rather than what it resolved to; they stay in
/// `ChartIdentity` beside the hash.
pub fn resolved_hash(chart: &Chart) -> [u8; 32] {
    let mut h = Encoder(Sha256::new());
    h.bytes(b"tapestry resolved chart");
    encode_chart(&mut h, chart);
    h.0.finalize().into()
}

/// The same encoding over the chart as loaded at `speed` other than 1×,
/// under its own tag and with the speed's k, so a record pins the real
/// timeline its input was judged on.
pub fn played_hash(chart: &Chart, speed: Speed) -> [u8; 32] {
    let mut h = Encoder(Sha256::new());
    h.bytes(b"tapestry played chart");
    h.u64(u64::from(speed.twentieths()));
    encode_chart(&mut h, chart);
    h.0.finalize().into()
}

fn encode_chart(h: &mut Encoder, chart: &Chart) {
    h.u64(u64::from(chart.identity.loader_version));
    h.i128(chart.scroll.initial_rate());
    let segments = chart.scroll.segments();
    h.len(segments.len());
    for s in segments {
        h.i64(s.start.0);
        h.i128(s.anchor.0);
        h.i128(s.slope);
    }
    for lane in &chart.lanes {
        h.len(lane.len());
        for note in lane {
            h.i64(note.time.0);
            h.i128(note.pos.0);
            h.sound(note.sound);
            match note.kind {
                NoteKind::Tap => h.u8(0),
                NoteKind::Long {
                    end,
                    end_pos,
                    end_sound,
                } => {
                    h.u8(1);
                    h.i64(end.0);
                    h.i128(end_pos.0);
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
        h.i128(bar.0);
    }
    h.len(chart.sounds.len());
    for sound in &chart.sounds {
        h.bytes(sound.name.as_bytes());
    }
    h.bytes(chart.meta.title.as_bytes());
    let headers = &chart.meta.judgment_headers;
    h.len(headers.len());
    for header in headers {
        h.u8(match header.name {
            JudgmentHeaderName::Rank => 0,
            JudgmentHeaderName::DefExRank => 1,
            JudgmentHeaderName::Total => 2,
        });
        match header.value {
            HeaderValue::Integer(v) => {
                h.u8(0);
                h.i128(v);
            }
            HeaderValue::Decimal(v) => {
                h.u8(1);
                h.i128(v);
            }
            HeaderValue::NotANumber => h.u8(2),
        }
    }
    h.u8(match chart.meta.ln_mode {
        None => 0,
        Some(LnMode::Ln) => 1,
        Some(LnMode::Cn) => 2,
        Some(LnMode::Hcn) => 3,
    });
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

    fn i128(&mut self, v: i128) {
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
