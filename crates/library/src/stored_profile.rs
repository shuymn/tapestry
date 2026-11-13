//! A chart's preview profile as the library stores it for the song list's
//! graphs (design.md "Loading and Song Selection", Summaries): its exact
//! MAIN tempo, its notes graph's bins and its tempo rows, in a bounded
//! encoding.
//!
//! The encoding is a version byte, a flags byte (bit 0: the rows were left
//! out), the MAIN tempo (a tag: 0 unavailable, 1 none weighted, 2 a tempo,
//! then for 2 the tempo as a row's), the content's length in whole ms
//! ([`ChartProfile::length_ms`]), the bin width in seconds, the bin count
//! and each bin's [`NOTE_CATEGORIES`] counts, then the row count and each
//! row: its time as the ms since the row before it, a tag byte (0 a STOP;
//! otherwise the tempo's power of ten plus 1) and the tempo's units over
//! that power, then its scroll factor as a byte (the power of ten times 2,
//! plus 1 when negative) and the factor's magnitude over that power. Every
//! number but the bytes is unsigned LEB128; the powers of ten keep a tempo
//! or factor of few significant digits (150 BPM, a factor of 1) to a few
//! bytes.
//!
//! At most [`MAX_ROWS`] rows and [`MAX_PROFILE_BYTES`] bytes are stored, both
//! inclusive; past either the rows are left out and the bins and MAIN kept,
//! so the notes graph stays where the tempo graph cannot. [`decode`] refuses
//! anything past those bounds, a value past its type, trailing bytes and a
//! truncated blob, and allocates only what the checked counts allow.

use bms_model::{ChartProfile, NOTE_BINS, NOTE_CATEGORIES, SpeedRow, Tempo};

/// The encoding's version.
const VERSION: u8 = 1;
/// The flag of rows left out.
const ROWS_LEFT_OUT: u8 = 1;
/// The most tempo rows stored.
pub const MAX_ROWS: usize = 4_096;
/// The most bytes a stored profile takes.
pub const MAX_PROFILE_BYTES: usize = 24 * 1_024;

/// A stored profile, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredProfile {
    /// The exact MAIN tempo ([`bms_model::main_tempo`]): `Some(None)` when
    /// no note weighs anything, `None` when it was unavailable.
    pub main: Option<Option<Tempo>>,
    /// The content's length in whole ms ([`ChartProfile::length_ms`]),
    /// which the tempo graph spans.
    pub length_ms: u32,
    /// The seconds each bin spans ([`ChartProfile::bin_seconds`]).
    pub bin_seconds: u32,
    /// The notes graph's bins ([`ChartProfile::bins`]).
    pub bins: Vec<[u16; NOTE_CATEGORIES]>,
    /// The tempo rows ([`ChartProfile::speeds`]); `None` when they were
    /// left out by the bounds.
    pub rows: Option<Vec<SpeedRow>>,
}

fn put(out: &mut Vec<u8>, mut v: u128) {
    loop {
        let byte = u8::try_from(v & 0x7f).unwrap_or(0);
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A magnitude as `(power, mantissa)`: the most trailing decimal zeros
/// taken off.
fn split(units: u128) -> (u8, u128) {
    let (mut power, mut mantissa) = (0_u8, units);
    while mantissa != 0 && mantissa % 10 == 0 && power < 38 {
        mantissa /= 10;
        power += 1;
    }
    (power, mantissa)
}

/// A tempo as a tag byte (its power of ten plus 1) and its mantissa.
fn put_tempo(out: &mut Vec<u8>, tempo: Tempo) {
    let (power, mantissa) = split(tempo.units());
    out.push(power + 1);
    put(out, mantissa);
}

fn count(n: usize) -> u128 {
    u128::try_from(n).unwrap_or(u128::MAX)
}

/// The stored form of `profile` with the MAIN tempo `main`, the rows left
/// out past [`MAX_ROWS`] or [`MAX_PROFILE_BYTES`]. `None` only for a profile
/// past [`NOTE_BINS`] bins, which no loaded chart has.
#[must_use]
pub fn encode(profile: &ChartProfile, main: Option<Option<Tempo>>) -> Option<Vec<u8>> {
    encode_parts(
        main,
        profile.length_ms(),
        profile.bin_seconds(),
        profile.bins(),
        profile.speeds(),
    )
}

/// [`encode`] of a profile's parts.
fn encode_parts(
    main: Option<Option<Tempo>>,
    length_ms: u32,
    bin_seconds: u32,
    bins: &[[u16; NOTE_CATEGORIES]],
    rows: &[SpeedRow],
) -> Option<Vec<u8>> {
    if bins.len() > NOTE_BINS {
        return None;
    }
    let mut out = Vec::new();
    out.push(VERSION);
    out.push(0);
    match main {
        None => out.push(0),
        Some(None) => out.push(1),
        Some(Some(t)) => {
            out.push(2);
            put_tempo(&mut out, t);
        }
    }
    put(&mut out, u128::from(length_ms));
    put(&mut out, u128::from(bin_seconds));
    put(&mut out, count(bins.len()));
    for &c in bins.iter().flatten() {
        put(&mut out, u128::from(c));
    }
    let without_rows = out.len();
    let mut fits = rows.len() <= MAX_ROWS;
    if fits {
        put(&mut out, count(rows.len()));
        let mut previous = 0;
        for row in rows {
            put(&mut out, u128::from(row.ms.saturating_sub(previous)));
            previous = row.ms;
            match row.tempo {
                None => out.push(0),
                Some(t) => put_tempo(&mut out, t),
            }
            let (power, mantissa) = split(row.scroll.unsigned_abs());
            out.push(power * 2 + u8::from(row.scroll < 0));
            put(&mut out, mantissa);
            if out.len() > MAX_PROFILE_BYTES {
                fits = false;
                break;
            }
        }
    }
    if !fits {
        out.truncate(without_rows);
        put(&mut out, 0);
        if let Some(flags) = out.get_mut(1) {
            *flags = ROWS_LEFT_OUT;
        }
    }
    out.shrink_to_fit();
    Some(out)
}

/// A reader over a stored profile's bytes.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    /// An LEB128 number that fits `u128`.
    fn number(&mut self) -> Option<u128> {
        let mut v = 0_u128;
        for shift in (0..128).step_by(7) {
            let b = self.byte()?;
            let part = u128::from(b & 0x7f);
            // The last group may hold only the bits left.
            if shift > 121 && part >> (128 - shift) != 0 {
                return None;
            }
            v |= part << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn bounded<T: TryFrom<u128>>(&mut self) -> Option<T> {
        T::try_from(self.number()?).ok()
    }

    /// `mantissa · 10^power`, if it fits.
    fn scaled(&mut self, power: u8) -> Option<u128> {
        let ten = 10_u128.checked_pow(u32::from(power))?;
        self.number()?.checked_mul(ten)
    }

    /// A tempo after its tag byte `tag` (not 0).
    fn tempo(&mut self, tag: u8) -> Option<Tempo> {
        Tempo::from_units(self.scaled(tag.checked_sub(1)?)?)
    }
}

/// The profile stored in `bytes`; `None` for anything this build did not
/// write within the bounds.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<StoredProfile> {
    if bytes.len() > MAX_PROFILE_BYTES {
        return None;
    }
    let mut r = Reader(bytes);
    if r.byte()? != VERSION {
        return None;
    }
    let flags = r.byte()?;
    if flags & !ROWS_LEFT_OUT != 0 {
        return None;
    }
    let main = match r.byte()? {
        0 => None,
        1 => Some(None),
        2 => {
            let tag = r.byte()?;
            Some(Some(r.tempo(tag)?))
        }
        _ => return None,
    };
    let length_ms: u32 = r.bounded()?;
    let bin_seconds: u32 = r.bounded()?;
    let bin_count: usize = r.bounded()?;
    if bin_count > NOTE_BINS {
        return None;
    }
    let mut bins = Vec::with_capacity(bin_count);
    for _ in 0..bin_count {
        let mut bin = [0_u16; NOTE_CATEGORIES];
        for c in &mut bin {
            *c = r.bounded()?;
        }
        bins.push(bin);
    }
    let row_count: usize = r.bounded()?;
    let left_out = flags & ROWS_LEFT_OUT != 0;
    if row_count > MAX_ROWS || (left_out && row_count != 0) {
        return None;
    }
    let rows = if left_out {
        None
    } else {
        let mut rows = Vec::with_capacity(row_count);
        let mut ms = 0_u32;
        for _ in 0..row_count {
            ms = ms.checked_add(r.bounded()?)?;
            let tempo = match r.byte()? {
                0 => None,
                tag => Some(r.tempo(tag)?),
            };
            let scroll_tag = r.byte()?;
            let magnitude = i128::try_from(r.scaled(scroll_tag / 2)?).ok()?;
            let scroll = if scroll_tag % 2 == 1 {
                magnitude.checked_neg()?
            } else {
                magnitude
            };
            rows.push(SpeedRow { ms, tempo, scroll });
        }
        Some(rows)
    };
    if !r.0.is_empty() {
        return None;
    }
    Some(StoredProfile {
        main,
        length_ms,
        bin_seconds,
        bins,
        rows,
    })
}

#[cfg(test)]
mod tests;
