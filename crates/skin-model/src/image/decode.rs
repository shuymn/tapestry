//! Capped reads, image headers and decoding: a whole file through
//! `image`, or an oversized PNG in bands of rows through `png`.
//!
//! No decoder is handed a file: bytes are read here, with every read
//! bounded, into an owned buffer the decoder then takes over.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::cell::Cell;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, Cursor, Read, Seek, SeekFrom};

use image::{
    DynamicImage, ImageBuffer, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits,
    RgbaImage,
};

use super::{HEADER_BYTES, MAX_IMAGE_TEXELS, MAX_SIDE, MAX_STREAMED_TEXELS, Unbanded};

/// The most a decoder may allocate while reading a header (chunk buffers
/// before the image data).
const HEADER_ALLOC: u64 = 4 << 20;
/// The widest native texel of an enabled format: 16-bit RGBA PNG.
pub(crate) const NATIVE_TEXEL_BYTES: u64 = 8;
/// Room above the native image for a decoder's own buffers.
const DECODE_MARGIN: u64 = 1 << 20;
/// A read buffer grows by at most this much at a time.
const READ_STEP: u64 = 1 << 20;
/// The rows of one band of a file decoded in bands.
pub(crate) const BAND_ROWS: u32 = 16;
/// What `png` may allocate by its own count while it decodes a file in
/// bands: the metadata it keeps and its output row.
pub(crate) const PNG_WORK_BYTES: usize = 1 << 20;

/// How a file is decoded, decided from its header at admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Whole, through `image`: at most [`MAX_IMAGE_TEXELS`], with a PNG's
    /// header fields (none for a JPEG or BMP).
    Full(Option<PngHeader>),
    /// In bands of [`BAND_ROWS`] rows: a non-interlaced, non-animated PNG
    /// past [`MAX_IMAGE_TEXELS`], with the header fields admission read.
    Banded(PngHeader),
}

/// A PNG header's fields that decide how its rows decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PngHeader {
    pub color: png::ColorType,
    pub depth: png::BitDepth,
    pub interlaced: bool,
}

/// What a header says: the dimensions, and how the file decodes, or why
/// a file past [`MAX_IMAGE_TEXELS`] cannot be decoded in bands.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Probe {
    pub dims: [u32; 2],
    pub mode: Result<Mode, Unbanded>,
}

/// Why a header or an image was not read.
#[derive(Debug)]
pub(crate) enum DecodeError {
    /// The header was not within the first [`HEADER_BYTES`].
    HeaderTooLarge,
    Unsupported,
    /// The dimensions differ from the ones expected.
    Changed,
    /// The format or a header field other than the dimensions differs
    /// from the ones admission read.
    HeaderChanged,
    /// `png` met its own allocation limit ([`PNG_WORK_BYTES`]).
    DecoderLimit,
    Image(ImageError),
    Png(png::DecodingError),
    Io(io::Error),
}

impl From<ImageError> for DecodeError {
    fn from(e: ImageError) -> Self {
        match e {
            ImageError::Unsupported(_) => Self::Unsupported,
            e => Self::Image(e),
        }
    }
}

impl From<png::DecodingError> for DecodeError {
    fn from(e: png::DecodingError) -> Self {
        match e {
            png::DecodingError::LimitsExceeded => Self::DecoderLimit,
            e => Self::Png(e),
        }
    }
}

/// Why a decode that may be cancelled stopped.
#[derive(Debug)]
pub(crate) enum Halt {
    Cancelled,
    Failed(DecodeError),
}

impl From<DecodeError> for Halt {
    fn from(e: DecodeError) -> Self {
        Self::Failed(e)
    }
}

/// Where a long build asks whether it was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    /// Each piece of a capped read.
    Read,
    /// Each band of rows.
    Band,
    /// Each read of the chunks after the image data.
    Trailing,
    /// Each mip level of each page.
    Mip,
}

#[cfg(test)]
thread_local! {
    /// The site this thread last asked at: a test's cancellation can
    /// answer for one site only.
    pub(crate) static SITE: Cell<Option<Site>> = const { Cell::new(None) };
}

/// The build was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Asks `cancel` at `site`.
pub(crate) fn checkpoint(cancel: &dyn Fn() -> bool, site: Site) -> Result<(), Cancelled> {
    #[cfg(test)]
    SITE.with(|s| s.set(Some(site)));
    #[cfg(not(test))]
    let _ = site;
    if cancel() { Err(Cancelled) } else { Ok(()) }
}

/// Whether `e` carries a cancellation out of a reader.
pub(crate) fn is_cancelled(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Cancelled>())
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
/// a time, asking `cancel` before each piece. Returns the bytes and whether
/// the file holds more; a cancellation is an error [`is_cancelled`] sees.
pub(crate) fn read_capped(
    file: &mut File,
    expect: u64,
    limit: u64,
    cancel: &dyn Fn() -> bool,
) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    reserve(&mut buf, expect.min(limit))?;
    let mut chunk = [0_u8; 16 * 1024];
    let mut reader = file.take(limit.saturating_add(1));
    loop {
        checkpoint(cancel, Site::Read).map_err(io::Error::other)?;
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
fn reader<R: BufRead + Seek>(bytes: R) -> Result<ImageReader<R>, DecodeError> {
    let reader = ImageReader::new(bytes)
        .with_guessed_format()
        .map_err(DecodeError::Io)?;
    if reader.format().is_none() {
        return Err(DecodeError::Unsupported);
    }
    Ok(reader)
}

/// Whether a file of `dims` passes [`MAX_IMAGE_TEXELS`] within the side
/// and [`MAX_STREAMED_TEXELS`] caps: the files decoded in bands when they
/// can be.
fn oversized([w, h]: [u32; 2]) -> bool {
    let texels = u64::from(w) * u64::from(h);
    w <= MAX_SIDE && h <= MAX_SIDE && texels > MAX_IMAGE_TEXELS && texels <= MAX_STREAMED_TEXELS
}

/// The header of `file` (`len` bytes long, at most [`HEADER_BYTES`] of it
/// read): its dimensions, and how it decodes. A file past
/// [`MAX_IMAGE_TEXELS`] within the other caps is read again, when it is a
/// PNG, by the banded decoder's own settings through the start of its
/// image data, for the fields of its banded decoding or why it has none.
pub(crate) fn header(file: &mut File, len: u64) -> Result<Probe, DecodeError> {
    let (bytes, more) = read_capped(file, len, HEADER_BYTES, &|| false).map_err(DecodeError::Io)?;
    let mut reader = reader(Cursor::new(bytes.as_slice()))?;
    let format = reader.format();
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(HEADER_ALLOC);
    reader.limits(limits);
    let dims = match reader.into_dimensions() {
        Ok((w, h)) => [w, h],
        Err(ImageError::Unsupported(_)) => return Err(DecodeError::Unsupported),
        Err(_) if more => return Err(DecodeError::HeaderTooLarge),
        Err(e) => return Err(DecodeError::Image(e)),
    };
    let png = format == Some(ImageFormat::Png);
    let mode = if !oversized(dims) {
        Ok(Mode::Full(if png {
            Some(png_ihdr(&bytes)?.1)
        } else {
            None
        }))
    } else if png {
        png_mode(&bytes)?
    } else {
        Err(Unbanded::Format)
    };
    Ok(Probe { dims, mode })
}

/// A `png` decoder over `r` as the banded path reads: its allocation
/// limit set before the header is parsed, text and colour profiles
/// skipped (their checksums still checked), every checksum verified and
/// an ancillary chunk's failure refused, and each row expanded to 8 or 16
/// bits a sample as `image` reads it.
fn png_decoder<R: BufRead + Seek>(r: R) -> png::Decoder<R> {
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(false);
    options.set_ignore_text_chunk(true);
    options.set_ignore_iccp_chunk(true);
    options.set_skip_ancillary_crc_failures(false);
    let mut decoder = png::Decoder::new_with_options(r, options);
    decoder.set_limits(png::Limits {
        bytes: PNG_WORK_BYTES,
    });
    decoder.set_transformations(png::Transformations::EXPAND);
    decoder
}

/// The dimensions and header fields in a PNG's `IHDR`.
fn png_ihdr(bytes: &[u8]) -> Result<([u32; 2], PngHeader), DecodeError> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: PNG_WORK_BYTES,
        },
    );
    let info = decoder.read_header_info()?;
    let (w, h) = info.size();
    let header = PngHeader {
        color: info.color_type,
        depth: info.bit_depth,
        interlaced: info.interlaced,
    };
    Ok(([w, h], header))
}

/// Checks `bytes`, a file admitted for a whole decode with `dims` and
/// `header`, before it is decoded: a PNG admitted must still be one with
/// the same dimensions and header fields, and a JPEG or BMP must not
/// have become a PNG.
///
/// # Errors
///
/// [`DecodeError::Changed`] for other dimensions,
/// [`DecodeError::HeaderChanged`] for another format or header field.
pub(crate) fn check_whole(
    bytes: &[u8],
    dims: [u32; 2],
    header: Option<PngHeader>,
) -> Result<(), DecodeError> {
    let format = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(DecodeError::Io)?
        .format();
    match (header, format == Some(ImageFormat::Png)) {
        (None, false) => Ok(()),
        (Some(_), false) | (None, true) => Err(DecodeError::HeaderChanged),
        (Some(header), true) => {
            let (now_dims, now) = png_ihdr(bytes)?;
            if now_dims != dims {
                Err(DecodeError::Changed)
            } else if now != header {
                Err(DecodeError::HeaderChanged)
            } else {
                Ok(())
            }
        }
    }
}

/// The banded mode of an oversized PNG's first bytes, or why it has none.
fn png_mode(bytes: &[u8]) -> Result<Result<Mode, Unbanded>, DecodeError> {
    let mut decoder = png_decoder(Cursor::new(bytes));
    let info = decoder.read_header_info()?;
    let header = PngHeader {
        color: info.color_type,
        depth: info.bit_depth,
        interlaced: info.interlaced,
    };
    if header.interlaced {
        return Ok(Err(Unbanded::Interlaced));
    }
    let reader = decoder.read_info()?;
    if reader.info().animation_control.is_some() {
        return Ok(Err(Unbanded::Animated));
    }
    Ok(Ok(Mode::Banded(header)))
}

/// Decodes `bytes`, which must hold an image of `dims`, into RGBA8 with
/// straight alpha.
///
/// Memory, for `E` encoded bytes and `T` texels: the decoder takes `bytes`
/// over; a JPEG decoder copies them while it is built and drops the
/// original (2E); decoding holds the encoded bytes and the native image
/// (E + 8T at most); the encoded bytes go with the decoder, then the
/// conversion holds the native image and the RGBA8 copy (12T at most, none
/// when the native image is already RGBA8). With `E` at most
/// [`MAX_FILE_BYTES`](super::MAX_FILE_BYTES) (64 MiB) and `T` at most
/// [`MAX_IMAGE_TEXELS`](super::MAX_IMAGE_TEXELS) (12 Mi), that is at most
/// 160 MiB.
pub(crate) fn decode(bytes: Vec<u8>, dims: [u32; 2]) -> Result<RgbaImage, DecodeError> {
    let [w, h] = dims;
    let native = u64::from(w) * u64::from(h) * NATIVE_TEXEL_BYTES;
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(native + DECODE_MARGIN);
    let mut reader = reader(Cursor::new(bytes))?;
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

/// Where the bands of a file decoded in bands go.
pub(crate) trait Bands {
    /// Whether anything draws rows `top..bottom`: a band nothing draws is
    /// decoded but not converted.
    fn wants(&mut self, top: u32, bottom: u32) -> bool;
    /// The band whose first row is `top`, in RGBA8 with straight alpha.
    fn put(&mut self, top: u32, band: &RgbaImage);
}

/// The encoded bytes as `png` reads them: once every row is read, each
/// read asks `cancel` first, the rest of the image data included.
struct Source<'a> {
    bytes: Cursor<Vec<u8>>,
    cancel: &'a dyn Fn() -> bool,
    trailing: &'a Cell<bool>,
}

impl Read for Source<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.bytes.read(buf)
    }
}

impl BufRead for Source<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.trailing.get() {
            checkpoint(self.cancel, Site::Trailing).map_err(io::Error::other)?;
        }
        self.bytes.fill_buf()
    }

    fn consume(&mut self, n: usize) {
        self.bytes.consume(n);
    }
}

impl Seek for Source<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.bytes.seek(pos)
    }
}

fn png_halt(e: png::DecodingError) -> Halt {
    match e {
        png::DecodingError::IoError(e) if is_cancelled(&e) => Halt::Cancelled,
        e => Halt::Failed(e.into()),
    }
}

fn out_of_memory(e: std::collections::TryReserveError) -> Halt {
    Halt::Failed(DecodeError::Io(io::Error::new(
        io::ErrorKind::OutOfMemory,
        e,
    )))
}

/// One band's native samples, as `png` expands them.
enum Samples {
    Eight(Vec<u8>),
    Sixteen(Vec<u16>),
}

/// Decodes `bytes`, a PNG admitted for banded decoding with `dims` and
/// `header`, row by row, handing each band of [`BAND_ROWS`] rows that
/// `out` wants to it in RGBA8, converted as [`decode`] converts a whole
/// image; then reads the chunks after the image data through `IEND`.
/// `cancel` is asked before each band and each read after the rows.
///
/// Memory, for `E` encoded bytes and a width `W` of at most [`MAX_SIDE`]:
/// the encoded bytes; `png`'s own buffers (its metadata and output row
/// within [`PNG_WORK_BYTES`], the inflater's tables and a window of
/// filtered rows); one expanded row (8W); and one band, native (8 × 16 W)
/// beside its RGBA8 copy (4 × 16 W).
///
/// # Errors
///
/// [`DecodeError::Changed`] when the dimensions differ from `dims`, and
/// [`DecodeError::HeaderChanged`] when the bytes are not a PNG, the
/// colour type, bit depth or interlacing differ from `header`, or the
/// file is now animated, all before any row is decoded; the decoder's
/// errors after that.
pub(crate) fn banded(
    bytes: Vec<u8>,
    dims: [u32; 2],
    header: PngHeader,
    cancel: &dyn Fn() -> bool,
    out: &mut dyn Bands,
) -> Result<(), Halt> {
    let format = ImageReader::new(Cursor::new(bytes.as_slice()))
        .with_guessed_format()
        .map_err(DecodeError::Io)?
        .format();
    if format != Some(ImageFormat::Png) {
        return Err(Halt::Failed(DecodeError::HeaderChanged));
    }
    let trailing = Cell::new(false);
    let source = Source {
        bytes: Cursor::new(bytes),
        cancel,
        trailing: &trailing,
    };
    let mut decoder = png_decoder(source);
    let info = decoder.read_header_info().map_err(png_halt)?;
    let [w, h] = dims;
    if info.size() != (w, h) {
        return Err(Halt::Failed(DecodeError::Changed));
    }
    let now = PngHeader {
        color: info.color_type,
        depth: info.bit_depth,
        interlaced: info.interlaced,
    };
    if now != header {
        return Err(Halt::Failed(DecodeError::HeaderChanged));
    }
    let mut reader = decoder.read_info().map_err(png_halt)?;
    if reader.info().animation_control.is_some() {
        return Err(Halt::Failed(DecodeError::HeaderChanged));
    }
    let (color, depth) = reader.output_color_type();
    let line = reader
        .output_line_size(w)
        .ok_or(Halt::Failed(DecodeError::DecoderLimit))?;
    let mut row = Vec::new();
    row.try_reserve_exact(line).map_err(out_of_memory)?;
    row.resize(line, 0);

    let mut top = 0;
    while top < h {
        checkpoint(cancel, Site::Band).map_err(|_| Halt::Cancelled)?;
        let bottom = top.saturating_add(BAND_ROWS).min(h);
        let rows = bottom - top;
        let mut band = if out.wants(top, bottom) {
            Some(samples(depth, line, rows)?)
        } else {
            None
        };
        for _ in 0..rows {
            if reader.read_row(&mut row).map_err(png_halt)?.is_none() {
                return Err(Halt::Failed(DecodeError::Png(png::DecodingError::IoError(
                    io::ErrorKind::UnexpectedEof.into(),
                ))));
            }
            match &mut band {
                Some(Samples::Eight(v)) => v.extend_from_slice(&row),
                Some(Samples::Sixteen(v)) => v.extend(
                    row.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&pair| u16::from_be_bytes(pair)),
                ),
                None => {}
            }
        }
        if let Some(band) = band {
            out.put(top, &to_rgba8(band, color, w, rows)?);
        }
        top = bottom;
    }
    // Every row read: from here each read asks `cancel`, as the reader is
    // exhausted (draining the rest of the image data chunks) and the
    // chunks after them are read through `IEND`.
    trailing.set(true);
    reader.read_row(&mut row).map_err(png_halt)?;
    reader.finish().map_err(png_halt)
}

/// An empty band of `rows` rows of `line` bytes, in samples of `depth`.
fn samples(depth: png::BitDepth, line: usize, rows: u32) -> Result<Samples, Halt> {
    let bytes = line.saturating_mul(usize::try_from(rows).unwrap_or(usize::MAX));
    Ok(if depth == png::BitDepth::Sixteen {
        let mut v = Vec::new();
        v.try_reserve_exact(bytes / 2).map_err(out_of_memory)?;
        Samples::Sixteen(v)
    } else {
        let mut v = Vec::new();
        v.try_reserve_exact(bytes).map_err(out_of_memory)?;
        Samples::Eight(v)
    })
}

/// A band of `w`×`rows` native samples of `color` in RGBA8, by the
/// conversion [`decode`] uses.
fn to_rgba8(band: Samples, color: png::ColorType, w: u32, rows: u32) -> Result<RgbaImage, Halt> {
    use png::ColorType as C;
    let bad = || Halt::Failed(DecodeError::Unsupported);
    let image = match (band, color) {
        (Samples::Eight(v), C::Grayscale) => {
            DynamicImage::ImageLuma8(ImageBuffer::from_raw(w, rows, v).ok_or_else(bad)?)
        }
        (Samples::Eight(v), C::GrayscaleAlpha) => {
            DynamicImage::ImageLumaA8(ImageBuffer::from_raw(w, rows, v).ok_or_else(bad)?)
        }
        (Samples::Eight(v), C::Rgb) => {
            DynamicImage::ImageRgb8(ImageBuffer::from_raw(w, rows, v).ok_or_else(bad)?)
        }
        (Samples::Eight(v), C::Rgba) => {
            DynamicImage::ImageRgba8(ImageBuffer::from_raw(w, rows, v).ok_or_else(bad)?)
        }
        (Samples::Sixteen(v), C::Grayscale) => {
            DynamicImage::ImageLuma16(ImageBuffer::from_raw(w, rows, v).ok_or_else(bad)?)
        }
        (Samples::Sixteen(v), C::GrayscaleAlpha) => {
            DynamicImage::ImageLumaA16(ImageBuffer::from_raw(w, rows, v).ok_or_else(bad)?)
        }
        (Samples::Sixteen(v), C::Rgb) => {
            DynamicImage::ImageRgb16(ImageBuffer::from_raw(w, rows, v).ok_or_else(bad)?)
        }
        (Samples::Sixteen(v), C::Rgba) => {
            DynamicImage::ImageRgba16(ImageBuffer::from_raw(w, rows, v).ok_or_else(bad)?)
        }
        // `EXPAND` leaves no indexed output.
        (_, C::Indexed) => return Err(bad()),
    };
    Ok(image.into_rgba8())
}
