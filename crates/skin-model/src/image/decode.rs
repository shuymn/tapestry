//! Capped reads, image headers and decoding: a whole PNG through `png`, a
//! whole JPEG or BMP through `image`, or an oversized PNG in bands of rows
//! through `png`.
//!
//! No decoder is handed a file: bytes are read here, with every read
//! bounded, into an owned buffer the decoder then takes over.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::cell::Cell;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, Cursor, Read, Seek, SeekFrom};

use image::{
    ColorType, DynamicImage, ImageBuffer, ImageDecoder, ImageError, ImageFormat, ImageReader,
    Limits, RgbaImage,
};

use super::{HEADER_BYTES, MAX_IMAGE_TEXELS, MAX_SIDE, MAX_STREAMED_TEXELS, Unbanded};

/// The most a decoder may allocate while reading a header (chunk buffers
/// before the image data).
const HEADER_ALLOC: u64 = 4 << 20;
/// The widest native texel of an enabled format: 16-bit RGBA PNG.
pub(crate) const NATIVE_TEXEL_BYTES: u64 = 8;
/// Room above the native image for a decoder's own buffers, when no
/// limit is given.
const DECODE_MARGIN: u64 = 1 << 20;
/// A read buffer grows by at most this much at a time.
const READ_STEP: u64 = 1 << 20;
/// The rows of one band of a file decoded in bands.
pub(crate) const BAND_ROWS: u32 = 16;
/// What `png` may allocate by its own count while it decodes a file in
/// bands, or whole under a limit: the metadata it keeps and its output
/// row.
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
    /// A buffer of the read or decode, or the decoder's own buffers by
    /// their bound, was past the limit given, or a buffer was refused by
    /// the allocator.
    Refused,
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
    /// After a chart image's decode ([`crate::chart_image`]).
    Decoded,
    /// After a chart image's fit into its slot.
    Fitted,
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

/// An I/O error of a read, [`DecodeError::Refused`] for a buffer the
/// allocator refused.
fn io_error(e: io::Error) -> DecodeError {
    if e.kind() == io::ErrorKind::OutOfMemory {
        DecodeError::Refused
    } else {
        DecodeError::Io(e)
    }
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
    let (bytes, more) = read_capped(file, len, HEADER_BYTES, &|| false).map_err(io_error)?;
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
/// Memory, for `E` encoded bytes and `T` texels: while decoding, the
/// encoded bytes, the decoder's own buffers ([`png_scratch`] and what `png`
/// counts, [`jpeg_scratch`], [`bmp_scratch`]) and the native image (8T at
/// most); then, while converting, the native image and its RGBA8 copy (12T
/// at most, none when the native image is already RGBA8); [`peak`] is the
/// larger. With `E` at most [`MAX_FILE_BYTES`](super::MAX_FILE_BYTES)
/// (64 MiB) and `T` at most [`MAX_IMAGE_TEXELS`](super::MAX_IMAGE_TEXELS)
/// (12 Mi), the buffers made here are at most 160 MiB. The native image and
/// the RGBA8 copy are allocated here, so a refusal is an error rather than
/// an abort; a decoder's own buffers are only bounded, and abort as any
/// out-of-memory does.
pub(crate) fn decode(bytes: Vec<u8>, dims: [u32; 2]) -> Result<RgbaImage, DecodeError> {
    decode_alpha(bytes, dims, None).map(|(image, _)| image)
}

/// [`decode`], and whether the decoded picture has an alpha channel (its
/// decoder's colour type: a PNG's alpha or transparency chunk, a BMP's
/// alpha mask).
///
/// With `limit`, the decode holds at most `limit` bytes at once: its
/// [`peak`], from the header and the file's length, is checked against it
/// before the decoder is built, `png` counts its own buffers within
/// [`PNG_WORK_BYTES`], and the native image and the RGBA8 copy are
/// allocated fallibly. Past it, or a buffer refused by the allocator, the
/// picture is [`DecodeError::Refused`].
pub(crate) fn decode_alpha(
    bytes: Vec<u8>,
    dims: [u32; 2],
    limit: Option<u64>,
) -> Result<(RgbaImage, bool), DecodeError> {
    let [w, h] = dims;
    let format = reader(Cursor::new(bytes.as_slice()))?.format();
    let header = if format == Some(ImageFormat::Png) {
        Some(png_ihdr(&bytes)?.1)
    } else {
        None
    };
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let (most, peak) = peak(format, header, len, dims)?;
    if limit.is_some_and(|l| peak > l) {
        return Err(DecodeError::Refused);
    }
    let (samples, layout) = if header.is_some() {
        png_whole(bytes, dims, most, limit.is_some())?
    } else {
        image_whole(bytes, dims, most, limit)?
    };
    Ok((rgba8(samples, layout, w, h)?, layout.alpha()))
}

/// The most the native image of a picture of `dims` in `format` (a PNG's
/// `header`) can take, and the most its decode from a file of `len` bytes
/// holds at once: while decoding, the file, the decoder's own buffers by
/// their bound and the native image; while converting, the native image and
/// its RGBA8 copy. The native image is taken at its largest for the format:
/// 3 bytes a texel for a JPEG, 4 for a BMP, and for a PNG its colour type
/// and depth expanded, with the alpha a transparency chunk may add.
fn peak(
    format: Option<ImageFormat>,
    header: Option<PngHeader>,
    len: u64,
    dims: [u32; 2],
) -> Result<(u64, u64), DecodeError> {
    let [w, h] = dims;
    let texels = u64::from(w) * u64::from(h);
    let (texel, decoder) = match (format, header) {
        // The file, kept by the decoder, and an `eXIf` chunk's copy of it.
        (Some(ImageFormat::Png), Some(header)) => (
            png_texel(header),
            png_scratch(header, w)
                .saturating_add(u64::try_from(PNG_WORK_BYTES).unwrap_or(u64::MAX))
                .saturating_add(len.saturating_mul(2)),
        ),
        // The file's copies are the envelope's.
        (Some(ImageFormat::Jpeg), None) => (3, jpeg_scratch(len, dims)),
        (Some(ImageFormat::Bmp), None) => (4, bmp_scratch(w).saturating_add(len)),
        _ => return Err(DecodeError::Unsupported),
    };
    let native = texels * texel;
    let decoding = decoder.saturating_add(native);
    let converting = native + texels * 4;
    Ok((native, decoding.max(converting)))
}

/// The bytes of a PNG texel of `header`'s colour type and depth as `png`
/// expands it, with the alpha a transparency chunk may add.
fn png_texel(header: PngHeader) -> u64 {
    let channels = match header.color {
        png::ColorType::Grayscale | png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb | png::ColorType::Rgba | png::ColorType::Indexed => 4,
    };
    if header.depth == png::BitDepth::Sixteen {
        channels * 2
    } else {
        channels
    }
}

/// The most `image`'s JPEG decoder allocates beside the native image for a
/// file of `len` bytes and `w`×`h` texels, by an envelope that reads
/// neither the file's sampling nor its scans: the copies of the input and
/// of its metadata segments (8 `len` and 64 KiB), the coefficient planes a
/// progressive or multi-scan file keeps (8 (w + 31)(h + 31)), and the rows
/// and upsampling buffers of the widest sampling (3 410 (w + 31)).
/// `zune-jpeg` takes no byte limit and allocates infallibly, so this is
/// their only bound; its constants follow `zune-jpeg` 0.5.15 under `image`
/// 0.25.10 (`the_decoder_bounds_follow_the_locked_versions`).
fn jpeg_scratch(len: u64, [w, h]: [u32; 2]) -> u64 {
    let [w, h] = [u64::from(w) + 31, u64::from(h) + 31];
    len.saturating_mul(8)
        .saturating_add(64 << 10)
        .saturating_add(8 * w * h)
        .saturating_add(3_410 * w)
}

/// The most `image`'s BMP decoder allocates beside the native image for a
/// picture `w` wide: its palette (768 bytes kept, 1 024 read) and one row
/// of 8-bit palette indices (4 ⌈w / 4⌉), or at most 256 bytes of a run in
/// a run-length file. It takes no byte limit (`image` 0.25.10).
fn bmp_scratch(w: u32) -> u64 {
    1_792 + 4 * u64::from(w.div_ceil(4))
}

/// The most `png` 0.18 allocates outside its own count while it decodes a
/// PNG `w` wide of `header`'s colour type and depth: the window of inflated
/// rows, twice the most it grows to (max(4R, 128 KiB) + 2R + 40 KiB for a
/// filtered row of R bytes), two rows of a stream cut short, a
/// deinterlaced row (8w), and the inflater's tables (32 KiB). Its own
/// count covers its chunk buffers and its output row.
fn png_scratch(header: PngHeader, w: u32) -> u64 {
    let samples: u64 = match header.color {
        png::ColorType::Grayscale | png::ColorType::Indexed => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
    };
    let bits = header.depth as u64;
    let row = 1 + (u64::from(w) * samples * bits).div_ceil(8);
    let window = (4 * row + 64).max(128 << 10) + 2 * row + (40 << 10);
    2 * window + 2 * row + 8 * u64::from(w) + (32 << 10)
}

/// A PNG decoded whole by `png` as `image` reads it, each row expanded to
/// 8 or 16 bits a sample, its text chunks and colour profile skipped, its
/// native image at most `most`. `png` counts its own buffers (its chunk
/// buffers, an `eXIf` chunk's among them, and its output row) within
/// [`PNG_WORK_BYTES`] when `limited`, past it [`DecodeError::Refused`];
/// otherwise within the native image's largest and 1 MiB.
fn png_whole(
    bytes: Vec<u8>,
    [w, h]: [u32; 2],
    most: u64,
    limited: bool,
) -> Result<(Vec<u8>, Layout), DecodeError> {
    let counted = if limited {
        PNG_WORK_BYTES
    } else {
        let native = u64::from(w) * u64::from(h) * NATIVE_TEXEL_BYTES;
        usize::try_from(native + DECODE_MARGIN).unwrap_or(usize::MAX)
    };
    let refused = |e: png::DecodingError| match e {
        png::DecodingError::LimitsExceeded if limited => DecodeError::Refused,
        e => e.into(),
    };
    let mut decoder =
        png::Decoder::new_with_limits(Cursor::new(bytes), png::Limits { bytes: counted });
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(refused)?;
    if reader.info().size() != (w, h) {
        return Err(DecodeError::Changed);
    }
    let (color, depth) = reader.output_color_type();
    let layout = Layout::of_png(color, depth)?;
    let samples = reader
        .output_buffer_size()
        .and_then(|n| u64::try_from(n).ok())
        .ok_or(DecodeError::Unsupported)?;
    if samples > most {
        return Err(DecodeError::Unsupported);
    }
    let mut buf = zeroed(samples)?;
    reader.next_frame(&mut buf).map_err(refused)?;
    Ok((buf, layout))
}

/// A JPEG or BMP decoded whole by `image`, its native image at most `most`,
/// its decoder's limit `limit` (or the native image and 1 MiB), which
/// these decoders apply to the dimensions alone.
fn image_whole(
    bytes: Vec<u8>,
    [w, h]: [u32; 2],
    most: u64,
    limit: Option<u64>,
) -> Result<(Vec<u8>, Layout), DecodeError> {
    let native = u64::from(w) * u64::from(h) * NATIVE_TEXEL_BYTES;
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(limit.unwrap_or(native + DECODE_MARGIN));
    let mut reader = reader(Cursor::new(bytes))?;
    reader.limits(limits);
    let decoder = reader.into_decoder()?;
    if decoder.dimensions() != (w, h) {
        return Err(DecodeError::Changed);
    }
    let samples = decoder.total_bytes();
    if samples > most {
        return Err(DecodeError::Unsupported);
    }
    let layout = Layout::of_image(decoder.color_type())?;
    let mut buf = zeroed(samples)?;
    decoder.read_image(&mut buf)?;
    Ok((buf, layout))
}

/// `n` zero bytes, [`DecodeError::Refused`] when the allocator refuses
/// them.
fn zeroed(n: u64) -> Result<Vec<u8>, DecodeError> {
    let n = usize::try_from(n).map_err(|_| DecodeError::Refused)?;
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| DecodeError::Refused)?;
    v.resize(n, 0);
    Ok(v)
}

/// How a decoded image's texels are laid out: grey, grey and alpha, RGB or
/// RGBA (`channels`), of 8-bit samples or of 16-bit ones in the byte order
/// `wide` reads.
#[derive(Clone, Copy)]
struct Layout {
    channels: usize,
    wide: Option<fn([u8; 2]) -> u16>,
}

impl Layout {
    /// The layout of `image`'s decoded `color` (16-bit samples in native
    /// byte order).
    fn of_image(color: ColorType) -> Result<Self, DecodeError> {
        let ne: fn([u8; 2]) -> u16 = u16::from_ne_bytes;
        let (channels, wide) = match color {
            ColorType::L8 => (1, None),
            ColorType::La8 => (2, None),
            ColorType::Rgb8 => (3, None),
            ColorType::Rgba8 => (4, None),
            ColorType::L16 => (1, Some(ne)),
            ColorType::La16 => (2, Some(ne)),
            ColorType::Rgb16 => (3, Some(ne)),
            ColorType::Rgba16 => (4, Some(ne)),
            _ => return Err(DecodeError::Unsupported),
        };
        Ok(Self { channels, wide })
    }

    /// The layout of `png`'s expanded output (16-bit samples big-endian).
    fn of_png(color: png::ColorType, depth: png::BitDepth) -> Result<Self, DecodeError> {
        let be: fn([u8; 2]) -> u16 = u16::from_be_bytes;
        let wide = match depth {
            png::BitDepth::Eight => None,
            png::BitDepth::Sixteen => Some(be),
            _ => return Err(DecodeError::Unsupported),
        };
        let channels = match color {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            // `EXPAND` leaves no indexed output.
            png::ColorType::Indexed => return Err(DecodeError::Unsupported),
        };
        Ok(Self { channels, wide })
    }

    /// Whether the texels carry alpha.
    const fn alpha(self) -> bool {
        self.channels.is_multiple_of(2)
    }

    /// The bytes of one texel.
    const fn bytes(self) -> usize {
        self.channels * if self.wide.is_some() { 2 } else { 1 }
    }
}

/// The native samples `buf` of a `w`×`h` image of `layout` in RGBA8:
/// `buf` itself when it already is, else converted texel by texel
/// ([`texel`]) into one allocation made here.
fn rgba8(buf: Vec<u8>, layout: Layout, w: u32, h: u32) -> Result<RgbaImage, DecodeError> {
    let bad = || DecodeError::Unsupported;
    if layout.channels == 4 && layout.wide.is_none() {
        return RgbaImage::from_raw(w, h, buf).ok_or_else(bad);
    }
    let texels = u64::from(w) * u64::from(h);
    let step = u64::try_from(layout.bytes()).map_err(|_| bad())?;
    if u64::try_from(buf.len()).ok() != texels.checked_mul(step) {
        return Err(bad());
    }
    let mut out = zeroed(texels * 4)?;
    for (src, dst) in buf
        .chunks_exact(layout.bytes())
        .zip(out.as_chunks_mut::<4>().0)
    {
        *dst = texel(src, layout);
    }
    drop(buf);
    RgbaImage::from_raw(w, h, out).ok_or_else(bad)
}

/// One texel `src` of `layout` in RGBA8, as `image` converts it: an 8-bit
/// sample as it is, a 16-bit one `x` to the nearest of `x / 257`
/// (`(x + 128) / 257`); grey goes to red, green and blue, and a missing
/// alpha is opaque.
fn texel(src: &[u8], layout: Layout) -> [u8; 4] {
    let mut s = [0_u8; 4];
    match layout.wide {
        None => {
            for (o, &v) in s.iter_mut().zip(src) {
                *o = v;
            }
        }
        Some(read) => {
            for (o, &pair) in s.iter_mut().zip(src.as_chunks::<2>().0) {
                *o = u8::try_from((u32::from(read(pair)) + 128) / 257).unwrap_or(u8::MAX);
            }
        }
    }
    let [a, b, c, d] = s;
    match layout.channels {
        1 => [a, a, a, u8::MAX],
        2 => [a, a, a, b],
        3 => [a, b, c, u8::MAX],
        _ => [a, b, c, d],
    }
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

/// A band of `w`×`rows` native samples of `color` in RGBA8, by `image`'s
/// conversion, which gives what [`decode`]'s own gives.
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageBuffer, ImageFormat, Luma, LumaA, Rgb, Rgba};

    use super::{DecodeError, decode_alpha};

    /// `image` encoded as `format`.
    fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    /// A sample that differs across the picture, of 16 bits.
    fn wide(x: u32, y: u32, k: u32) -> u16 {
        u16::try_from((x * 1031 + y * 977 + k * 4099) % 65_536).unwrap()
    }

    fn narrow(x: u32, y: u32, k: u32) -> u8 {
        u8::try_from((x * 7 + y * 13 + k * 61) % 256).unwrap()
    }

    /// The conversion done here, texel by texel into its own allocation,
    /// gives `image`'s own result for every colour type an enabled format
    /// decodes to, and for every 8-bit and 16-bit sample value.
    #[test]
    fn the_bounded_conversion_matches_image_s() {
        let [w, h] = [37, 41];
        let images = [
            DynamicImage::ImageLuma8(ImageBuffer::from_fn(w, h, |x, y| Luma([narrow(x, y, 0)]))),
            DynamicImage::ImageLumaA8(ImageBuffer::from_fn(w, h, |x, y| {
                LumaA([narrow(x, y, 0), narrow(x, y, 1)])
            })),
            DynamicImage::ImageRgb8(ImageBuffer::from_fn(w, h, |x, y| {
                Rgb([0, 1, 2].map(|k| narrow(x, y, k)))
            })),
            DynamicImage::ImageRgba8(ImageBuffer::from_fn(w, h, |x, y| {
                Rgba([0, 1, 2, 3].map(|k| narrow(x, y, k)))
            })),
            DynamicImage::ImageLuma16(ImageBuffer::from_fn(w, h, |x, y| Luma([wide(x, y, 0)]))),
            DynamicImage::ImageLumaA16(ImageBuffer::from_fn(w, h, |x, y| {
                LumaA([wide(x, y, 0), wide(x, y, 1)])
            })),
            DynamicImage::ImageRgb16(ImageBuffer::from_fn(w, h, |x, y| {
                Rgb([0, 1, 2].map(|k| wide(x, y, k)))
            })),
            DynamicImage::ImageRgba16(ImageBuffer::from_fn(w, h, |x, y| {
                Rgba([0, 1, 2, 3].map(|k| wide(x, y, k)))
            })),
        ];
        let mut files: Vec<_> = images
            .iter()
            .map(|i| (format!("{:?} png", i.color()), encoded(i, ImageFormat::Png)))
            .collect();
        files.push(("rgb bmp".into(), encoded(&images[2], ImageFormat::Bmp)));
        files.push(("rgb jpeg".into(), encoded(&images[2], ImageFormat::Jpeg)));
        files.push(("luma jpeg".into(), encoded(&images[0], ImageFormat::Jpeg)));
        // Every sample value: 8 bits in grey and alpha, 16 bits in grey.
        let every8 = DynamicImage::ImageLumaA8(ImageBuffer::from_fn(256, 1, |x, _| {
            let v = u8::try_from(x).unwrap();
            LumaA([v, u8::MAX - v])
        }));
        let every16 = DynamicImage::ImageLuma16(ImageBuffer::from_fn(256, 256, |x, y| {
            Luma([u16::try_from(y * 256 + x).unwrap()])
        }));
        let every = [
            ("every 8-bit value", every8, [256, 1]),
            ("every 16-bit value", every16, [256, 256]),
        ];
        let files = files
            .into_iter()
            .map(|(what, bytes)| (what, bytes, [w, h]))
            .chain(
                every
                    .into_iter()
                    .map(|(what, i, dims)| (what.into(), encoded(&i, ImageFormat::Png), dims)),
            );
        for (what, bytes, dims) in files {
            let theirs = image::load_from_memory(&bytes).unwrap();
            let alpha = theirs.color().has_alpha();
            let theirs = theirs.into_rgba8();
            let (ours, has) = decode_alpha(bytes, dims, None).unwrap();
            assert!(ours == theirs, "{what}");
            assert_eq!(has, alpha, "{what}");
        }
    }

    /// A picture's name, file and dimensions, and the edge of its decode
    /// from the file's length.
    type Case<'a> = (&'a str, Vec<u8>, [u32; 2], &'a dyn Fn(u64) -> u64);

    /// With a limit, a decode is refused before its decoder is built when
    /// the most it holds at once passes the limit by one byte, and decoded
    /// at it, whichever phase holds the most: while decoding, the file, the
    /// decoder's own buffers by their bound (a JPEG's envelope, a BMP's
    /// palette and row, a PNG's window of rows and its own count) and the
    /// native image at its largest for the format; while converting, the
    /// native image and its RGBA8 copy.
    #[test]
    fn a_limit_holds_each_phase_of_a_decode_at_its_edge() {
        let small = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(16, 2, Rgb([9, 8, 7])));
        let large = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(640, 480, Rgb([9, 8, 7])));
        // Each edge worked out from the bounds' documented terms, not by
        // calling them, so a term left out of a bound moves the bound and
        // not the edge.
        let cases: [Case<'_>; 4] = [
            // Decoding holds the most: the decoder's own buffers outweigh
            // the picture. A JPEG's envelope: 8 times the file, 64 KiB,
            // 8 (16 + 31)(2 + 31) and 3 410 (16 + 31); its native image
            // 3 bytes a texel.
            (
                "JPEG",
                encoded(&small, ImageFormat::Jpeg),
                [16, 2],
                &|len| 8 * len + 65_536 + 8 * 47 * 33 + 3_410 * 47 + 16 * 2 * 3,
            ),
            // A BMP's file, palette (1 792) and row of 4 ⌈16 / 4⌉; its
            // native image at its largest, 4 bytes a texel.
            ("BMP", encoded(&small, ImageFormat::Bmp), [16, 2], &|len| {
                len + 1_792 + 16 + 16 * 2 * 4
            }),
            // A PNG's file twice, its window twice (a filtered row R of
            // 1 + 16 × 3 = 49 bytes: max(4R + 64, 128 KiB) + 2R + 40 KiB =
            // 172 130), two rows, a deinterlaced row (8 × 16) and the
            // inflater's tables (32 KiB), `png`'s own 1 MiB, and its native
            // image at its largest for RGB, 4 bytes a texel.
            ("PNG", encoded(&small, ImageFormat::Png), [16, 2], &|len| {
                2 * len + 2 * 172_130 + 2 * 49 + 8 * 16 + 32_768 + 1_048_576 + 16 * 2 * 4
            }),
            // Converting holds the most: the native image, at a BMP's
            // largest, beside its RGBA8 copy.
            (
                "large BMP",
                encoded(&large, ImageFormat::Bmp),
                [640, 480],
                &|_| 2 * 640 * 480 * 4,
            ),
        ];
        for (what, bytes, dims, edge) in cases {
            let edge = edge(u64::try_from(bytes.len()).unwrap());
            let decode = |limit| decode_alpha(bytes.clone(), dims, Some(limit));
            assert!(decode(edge).is_ok(), "{what} at {edge}");
            assert!(
                matches!(decode(edge - 1), Err(DecodeError::Refused)),
                "{what} below {edge}"
            );
        }
    }

    /// The PNG CRC of `bytes`.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut c = !0_u32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 == 0 {
                    c >> 1
                } else {
                    0xEDB8_8320 ^ (c >> 1)
                };
            }
        }
        !c
    }

    /// With a limit, `png` counts its own buffers within
    /// [`PNG_WORK_BYTES`]: a PNG whose `eXIf` chunk is past it is refused,
    /// though the most its decode holds is within the limit; without a
    /// limit `png` counts the native image and 1 MiB, and it decodes.
    #[test]
    fn a_limit_holds_png_s_own_count() {
        let picture = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(640, 480, Rgb([9, 8, 7])));
        let mut bytes = encoded(&picture, ImageFormat::Png);
        let data = vec![0_u8; 2 << 20];
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        let mut body = b"eXIf".to_vec();
        body.extend_from_slice(&data);
        chunk.extend_from_slice(&body);
        chunk.extend_from_slice(&crc32(&body).to_be_bytes());
        // After the signature (8 bytes) and `IHDR` (25).
        bytes.splice(33..33, chunk);
        assert!(matches!(
            decode_alpha(bytes.clone(), [640, 480], Some(161 << 20)),
            Err(DecodeError::Refused)
        ));
        assert!(decode_alpha(bytes, [640, 480], None).is_ok());
    }

    /// The bounds of the decoders' own buffers ([`jpeg_scratch`],
    /// [`bmp_scratch`], [`png_scratch`]) were read from these versions'
    /// sources: a lock file that moves any of them fails here until they
    /// are read again.
    #[test]
    fn the_decoder_bounds_follow_the_locked_versions() {
        let lock = include_str!("../../../../Cargo.lock").replace("\r\n", "\n");
        for (name, version) in [
            ("image", "0.25.10"),
            ("png", "0.18.1"),
            ("fdeflate", "0.3.7"),
            ("zune-jpeg", "0.5.15"),
            ("zune-core", "0.5.3"),
        ] {
            let named = format!("name = \"{name}\"\n");
            assert_eq!(lock.matches(&named).count(), 1, "one {name}");
            assert!(
                lock.contains(&format!("{named}version = \"{version}\"\n")),
                "{name} {version}"
            );
        }
    }
}
