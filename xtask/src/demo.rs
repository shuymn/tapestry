//! `cargo xtask demo-chart <dir>`: a self-made chart of about two minutes with
//! generated sounds, for manual runs of the game (design.md "Testing
//! Strategy"). Nothing here ships, and nothing it writes enters the repository.
//!
//! The chart runs at 150 BPM, 1.6 s a measure, measures 1 to 75 (the last
//! note at 121.4 s). Measures 1-16 hold quarter notes, 17-48 eighth notes,
//! 49-75 eighth notes with a second key on beats 1 and 3, and every odd
//! measure from 17 a scratch note on its first beat. The BGM is a kick on
//! every beat, a hat on every off-beat, and a pad chord every four
//! measures. Each lane has its own key sound.

use std::f32::consts::TAU;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Sample rate of the generated sounds.
const RATE: u32 = 44_100;
/// The last measure with notes.
const LAST_MEASURE: u16 = 75;
/// Eighth-note slots in a measure.
const SLOTS: usize = 8;

/// Key sound ids: `03` to `09` for keys 1-7, `0A` for the scratch.
const KEY_SOUNDS: [&str; 8] = ["0A", "03", "04", "05", "06", "07", "08", "09"];
/// The 1P channel of each lane: the scratch is `16`, keys 1-5 are `11`-`15`,
/// keys 6 and 7 are `18` and `19`.
const LANE_CHANNELS: [&str; 8] = ["16", "11", "12", "13", "14", "15", "18", "19"];

/// Writes `demo.bms` and its sounds into `dir`, creating it, and returns
/// the chart's path.
///
/// # Errors
///
/// The first file that cannot be written.
pub fn write(dir: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    // A pentatonic scale over two octaves for the keys, the scratch lower.
    let keys = [196.0, 261.63, 293.66, 329.63, 392.0, 440.0, 523.25, 587.33];
    for (lane, &freq) in keys.iter().enumerate() {
        let samples = if lane == 0 {
            sweep(freq, 90)
        } else {
            tone(freq, 180)
        };
        write_wav(&dir.join(format!("key{lane}.wav")), &samples)?;
    }
    write_wav(&dir.join("kick.wav"), &kick())?;
    write_wav(&dir.join("hat.wav"), &hat())?;
    write_wav(&dir.join("pad.wav"), &pad())?;
    let path = dir.join("demo.bms");
    fs::write(&path, chart_text())?;
    Ok(path)
}

/// The key lane of slot `slot` in measure `m`, 1-7.
fn key_lane(m: u16, slot: usize) -> usize {
    (usize::from(m) * 5 + slot * 3) % 7 + 1
}

/// The chart text.
fn chart_text() -> String {
    let mut t =
        String::from("#PLAYER 1\n#TITLE Demo\n#ARTIST tapestry\n#BPM 150\n#RANK 2\n#TOTAL 300\n");
    t.push_str("#WAV01 kick.wav\n#WAV02 hat.wav\n#WAV0B pad.wav\n");
    for (lane, id) in KEY_SOUNDS.iter().enumerate() {
        let _ = writeln!(t, "#WAV{id} key{lane}.wav");
    }
    for m in 1..=LAST_MEASURE {
        // BGM: a kick on each beat and a hat on each off-beat, on two
        // lines of channel 01, and the pad every four measures.
        let _ = writeln!(t, "#{m:03}01:01010101");
        let _ = writeln!(t, "#{m:03}01:0002000200020002");
        if (m - 1) % 4 == 0 {
            let _ = writeln!(t, "#{m:03}01:0B");
        }
        // Lanes by slots; a slot can set two lanes, so it is the index.
        let mut grid = [[false; SLOTS]; 8];
        #[allow(clippy::needless_range_loop)]
        for slot in 0..SLOTS {
            let used = match m {
                1..=16 => slot % 2 == 0,
                _ => true,
            };
            if !used {
                continue;
            }
            let key = key_lane(m, slot);
            grid[key][slot] = true;
            if m >= 49 && slot % 4 == 0 {
                grid[(key - 1 + 3) % 7 + 1][slot] = true;
            }
        }
        if m >= 17 && m % 2 == 1 {
            grid[0][0] = true;
        }
        for (lane, slots) in grid.iter().enumerate() {
            if !slots.contains(&true) {
                continue;
            }
            let _ = write!(t, "#{m:03}{}:", LANE_CHANNELS[lane]);
            for &on in slots {
                t.push_str(if on { KEY_SOUNDS[lane] } else { "00" });
            }
            t.push('\n');
        }
    }
    t
}

/// `ms` milliseconds of silence to fill.
fn frames(ms: u32) -> usize {
    (RATE * ms / 1000) as usize
}

/// A plucked tone: a sine with its second harmonic, decaying.
fn tone(freq: f32, ms: u32) -> Vec<f32> {
    let n = frames(ms);
    (0..n)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            let env = (-t * 18.0).exp();
            env * (0.6 * (TAU * freq * t).sin() + 0.2 * (TAU * 2.0 * freq * t).sin())
        })
        .collect()
}

/// A falling sweep with noise, for the scratch.
fn sweep(freq: f32, ms: u32) -> Vec<f32> {
    let mut noise = Noise(0x2545_f491);
    let n = frames(ms);
    let mut phase = 0.0_f32;
    (0..n)
        .map(|i| {
            let k = i as f32 / n as f32;
            phase += TAU * freq * (3.0 - 2.0 * k) / RATE as f32;
            (1.0 - k) * (0.5 * phase.sin() + 0.2 * noise.next())
        })
        .collect()
}

/// A kick: a sine falling from 150 Hz to 50 Hz.
fn kick() -> Vec<f32> {
    let n = frames(140);
    let mut phase = 0.0_f32;
    (0..n)
        .map(|i| {
            let k = i as f32 / n as f32;
            phase += TAU * (150.0 - 100.0 * k) / RATE as f32;
            0.8 * (1.0 - k) * phase.sin()
        })
        .collect()
}

/// A hat: 30 ms of decaying noise.
fn hat() -> Vec<f32> {
    let mut noise = Noise(0x9e37_79b9);
    let n = frames(30);
    (0..n)
        .map(|i| 0.25 * (1.0 - i as f32 / n as f32) * noise.next())
        .collect()
}

/// A pad: a quiet chord over four measures (6.4 s), faded in and out.
fn pad() -> Vec<f32> {
    let n = frames(6_400);
    (0..n)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            let k = i as f32 / n as f32;
            let env = (k * 10.0).min(1.0) * ((1.0 - k) * 4.0).min(1.0);
            let chord = [130.81, 164.81, 196.0]
                .iter()
                .map(|f| (TAU * f * t).sin())
                .sum::<f32>();
            0.08 * env * chord
        })
        .collect()
}

/// A fixed pseudo-random sequence in -1..1 (xorshift32), so the sounds are
/// the same on every run.
struct Noise(u32);

impl Noise {
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x as f32 / u32::MAX as f32 * 2.0 - 1.0
    }
}

/// Writes `samples` (-1..1) as a mono 16-bit PCM WAV file.
fn write_wav(path: &Path, samples: &[f32]) -> std::io::Result<()> {
    let data = u32::try_from(samples.len() * 2).expect("generated sounds are short");
    let mut b = Vec::with_capacity(44 + samples.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16_u32.to_le_bytes());
    b.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1_u16.to_le_bytes()); // mono
    b.extend_from_slice(&RATE.to_le_bytes());
    b.extend_from_slice(&(RATE * 2).to_le_bytes());
    b.extend_from_slice(&2_u16.to_le_bytes());
    b.extend_from_slice(&16_u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    fs::write(path, b)
}

#[cfg(test)]
mod tests {
    use bms_model::{NoteKind, TimeUs};
    use bms_parser::{LoadOptions, load_bms};

    #[test]
    fn demo_chart_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = super::write(dir.path()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let loaded = load_bms(&bytes, dir.path(), &LoadOptions::default()).unwrap();
        let chart = &loaded.chart;
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        // 11 sounds, every one resolved inside the folder.
        assert_eq!(chart.sounds.len(), 11);
        for s in &chart.sounds {
            let p = s
                .path
                .as_ref()
                .unwrap_or_else(|| panic!("{} unresolved", s.name));
            assert!(p.starts_with(std::fs::canonicalize(dir.path()).unwrap()));
        }
        // Worked out from the pattern: 16 × 4 + 32 × 8 + 27 × 10 keys and
        // 30 scratch notes (odd measures 17-75).
        let notes: Vec<_> = chart.lanes.iter().flatten().collect();
        assert!(notes.iter().all(|n| n.kind == NoteKind::Tap));
        assert_eq!(chart.lanes[0].len(), 30);
        assert_eq!(notes.len(), 64 + 256 + 270 + 30);
        // 300 kicks, 300 hats, a pad every 4 measures from 1 to 73.
        assert_eq!(chart.bgm.len(), 300 + 300 + 19);
        // The last note: measure 75, slot 7, at 75 × 1.6 s + 7 × 0.2 s.
        let last = notes.iter().map(|n| n.time).max().unwrap();
        assert_eq!(last, TimeUs(121_400_000));
    }
}
