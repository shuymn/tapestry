//! Capped reads, image headers and decoding through `image`.
//!
//! No decoder is handed a file: bytes are read here, with every read
//! bounded, into an owned buffer the decoder then takes over.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fs::File;
use std::io::{self, Cursor, Read};

use image::{DynamicImage, ImageDecoder, ImageError, ImageReader, Limits, RgbaImage};

use super::HEADER_BYTES;

/// The most a decoder may allocate while reading a header (chunk buffers
/// before the image data).
const HEADER_ALLOC: u64 = 4 << 20;
/// The widest native texel of an enabled format: 16-bit RGBA PNG.
pub(crate) const NATIVE_TEXEL_BYTES: u64 = 8;
/// Room above the native image for a decoder's own buffers.
const DECODE_MARGIN: u64 = 1 << 20;
/// A read buffer grows by at most this much at a time.
const READ_STEP: u64 = 1 << 20;

/// Why a header or an image was not read.
#[derive(Debug)]
pub(crate) enum DecodeError {
    /// The header was not within the first [`HEADER_BYTES`].
    HeaderTooLarge,
    Unsupported,
    /// The dimensions differ from the ones expected.
    Changed,
    Image(ImageError),
    Io(io::Error),
}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ImageError> for DecodeError {
    fn from(e: ImageError) -> Self {
        match e {
            ImageError::Unsupported(_) => Self::Unsupported,
            e => Self::Image(e),
        }
    }
}

fn to_usize(n: u64) -> io::Result<usize> {
    usize::try_from(n).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
}

fn reserve(buf: &mut Vec<u8>, n: u64) -> io::Result<()> {
    buf.try_reserve_exact(to_usize(n)?)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))
}

/// Reads at most `limit` bytes of `file` into a buffer reserved for
/// `expect` bytes (capped at `limit`) and grown by at most [`READ_STEP`] at
/// a time. Returns the bytes and whether the file holds more.
pub(crate) fn read_capped(file: &mut File, expect: u64, limit: u64) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    reserve(&mut buf, expect.min(limit))?;
    let mut chunk = [0_u8; 16 * 1024];
    let mut reader = file.take(limit.saturating_add(1));
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => return Ok((buf, false)),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let have = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        let room = limit.saturating_sub(have);
        let got = u64::try_from(n).unwrap_or(u64::MAX);
        let keep = got.min(room);
        let spare = u64::try_from(buf.capacity() - buf.len()).unwrap_or(u64::MAX);
        if spare < keep {
            reserve(&mut buf, keep.max(READ_STEP).min(room))?;
        }
        let Some(part) = chunk.get(..to_usize(keep)?) else {
            return Ok((buf, true));
        };
        buf.extend_from_slice(part);
        if got > keep {
            return Ok((buf, true));
        }
    }
}

/// A reader over `bytes` with its format guessed from the content.
fn reader(bytes: Vec<u8>) -> Result<ImageReader<Cursor<Vec<u8>>>, DecodeError> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    if reader.format().is_none() {
        return Err(DecodeError::Unsupported);
    }
    Ok(reader)
}

/// The width and height in the header of `file` (`len` bytes long, at most
/// [`HEADER_BYTES`] of it read).
pub(crate) fn header(file: &mut File, len: u64) -> Result<[u32; 2], DecodeError> {
    let (bytes, more) = read_capped(file, len, HEADER_BYTES)?;
    let mut reader = reader(bytes)?;
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(HEADER_ALLOC);
    reader.limits(limits);
    match reader.into_dimensions() {
        Ok((w, h)) => Ok([w, h]),
        Err(ImageError::Unsupported(_)) => Err(DecodeError::Unsupported),
        Err(_) if more => Err(DecodeError::HeaderTooLarge),
        Err(e) => Err(DecodeError::Image(e)),
    }
}

/// Decodes `bytes`, which must hold an image of `dims`, into RGBA8 with
/// straight alpha.
///
/// Memory, for `E` encoded bytes and `T` texels: the decoder takes `bytes`
/// over; a JPEG decoder copies them while it is built and drops the
/// original (2E); decoding holds the encoded bytes and the native image
/// (E + 8T at most); the encoded bytes go with the decoder, then the
/// conversion holds the native image and the RGBA8 copy (12T at most, none
/// when the native image is already RGBA8).
pub(crate) fn decode(bytes: Vec<u8>, dims: [u32; 2]) -> Result<RgbaImage, DecodeError> {
    let [w, h] = dims;
    let native = u64::from(w) * u64::from(h) * NATIVE_TEXEL_BYTES;
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(native + DECODE_MARGIN);
    let mut reader = reader(bytes)?;
    reader.limits(limits);
    let decoder = reader.into_decoder()?;
    if decoder.dimensions() != (w, h) {
        return Err(DecodeError::Changed);
    }
    if decoder.total_bytes() > native {
        return Err(DecodeError::Unsupported);
    }
    Ok(DynamicImage::from_decoder(decoder)?.into_rgba8())
}
