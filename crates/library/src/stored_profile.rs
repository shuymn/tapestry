//! A chart's profile as the library stores it for the song list's graphs
//! (design.md "Loading and Song Selection", Summaries): its notes graph's
//! bins and its tempo rows, in a bounded encoding.
//!
//! The encoding is a version byte, a flags byte (bit 0: the rows were left
//! out), the bin width in seconds, the bin count and each bin's
//! [`NOTE_CATEGORIES`] counts, then the row count and each row: its time as
//! the ms since the row before it, a tag byte (0 a STOP; otherwise the
//! tempo's power of ten plus 1) and the tempo's units over that power. Every
//! number but the bytes is unsigned LEB128.
//!
//! At most [`MAX_ROWS`] rows and [`MAX_PROFILE_BYTES`] bytes are stored; past
//! either the rows are left out and the bins kept, so the notes graph stays
//! where the tempo graph cannot. [`decode`] refuses anything past those
//! bounds, a value past its type, trailing bytes and a truncated blob, and
//! allocates only what the checked counts allow.

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

/// The bytes of `v` in LEB128.
fn len_of(v: u128) -> usize {
    let bits = 128 - usize::try_from(v.leading_zeros()).unwrap_or(128);
    bits.div_ceil(7).max(1)
}

/// A tempo's units as `(power, mantissa)`: the most trailing decimal zeros
/// taken off, so a tempo of a few significant digits takes a few bytes.
fn split(units: u128) -> (u8, u128) {
    let (mut power, mut mantissa) = (0_u8, units);
    while mantissa != 0 && mantissa % 10 == 0 && power < 38 {
        mantissa /= 10;
        power += 1;
    }
    (power, mantissa)
}

/// The bytes one row takes after `previous` ms.
fn row_len(row: &SpeedRow, previous: u32) -> usize {
    let delta = len_of(u128::from(row.ms.saturating_sub(previous)));
    delta + 1 + row.tempo.map_or(0, |t| len_of(split(t.units()).1))
}

/// The stored form of `profile`, the rows left out past [`MAX_ROWS`] or
/// [`MAX_PROFILE_BYTES`]. `None` only for a profile past [`NOTE_BINS`]
/// bins, which no loaded chart has.
#[must_use]
pub fn encode(profile: &ChartProfile) -> Option<Vec<u8>> {
    encode_parts(profile.bin_seconds(), profile.bins(), profile.speeds())
}

/// [`encode`] of a profile's parts.
fn encode_parts(
    bin_seconds: u32,
    bins: &[[u16; NOTE_CATEGORIES]],
    rows: &[SpeedRow],
) -> Option<Vec<u8>> {
    if bins.len() > NOTE_BINS {
        return None;
    }
    let bin_seconds = u128::from(bin_seconds);
    let count = |n: usize| u128::try_from(n).unwrap_or(u128::MAX);
    let head = 2 + len_of(bin_seconds) + len_of(count(bins.len()));
    let bins_len: usize = bins.iter().flatten().map(|&c| len_of(u128::from(c))).sum();
    let mut previous = 0;
    let rows_len: usize = rows
        .iter()
        .map(|r| {
            let len = row_len(r, previous);
            previous = r.ms;
            len
        })
        .sum();
    let with_rows = head + bins_len + len_of(count(rows.len())) + rows_len;
    let keep_rows = rows.len() <= MAX_ROWS && with_rows <= MAX_PROFILE_BYTES;
    let size = if keep_rows {
        with_rows
    } else {
        head + bins_len + 1
    };
    let mut out = Vec::with_capacity(size);
    out.push(VERSION);
    out.push(if keep_rows { 0 } else { ROWS_LEFT_OUT });
    put(&mut out, bin_seconds);
    put(&mut out, count(bins.len()));
    for &c in bins.iter().flatten() {
        put(&mut out, u128::from(c));
    }
    if keep_rows {
        put(&mut out, count(rows.len()));
        let mut previous = 0;
        for row in rows {
            put(&mut out, u128::from(row.ms.saturating_sub(previous)));
            previous = row.ms;
            match row.tempo {
                None => out.push(0),
                Some(t) => {
                    let (power, mantissa) = split(t.units());
                    out.push(power + 1);
                    put(&mut out, mantissa);
                }
            }
        }
    } else {
        put(&mut out, 0);
    }
    debug_assert_eq!(out.len(), size);
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
    if row_count > MAX_ROWS || (flags & ROWS_LEFT_OUT != 0 && row_count != 0) {
        return None;
    }
    let rows = if flags & ROWS_LEFT_OUT == 0 {
        let mut rows = Vec::with_capacity(row_count);
        let mut ms = 0_u32;
        for _ in 0..row_count {
            ms = ms.checked_add(r.bounded()?)?;
            let tempo = match r.byte()? {
                0 => None,
                tag => {
                    let ten = 10_u128.checked_pow(u32::from(tag - 1))?;
                    let units = r.number()?.checked_mul(ten)?;
                    Some(Tempo::from_units(units)?)
                }
            };
            rows.push(SpeedRow { ms, tempo });
        }
        Some(rows)
    } else {
        None
    };
    if !r.0.is_empty() {
        return None;
    }
    Some(StoredProfile {
        bin_seconds,
        bins,
        rows,
    })
}

#[cfg(test)]
mod tests;
