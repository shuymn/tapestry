//! Every call the worker makes into FFmpeg: the one module that holds the
//! worker's unsafe FFmpeg code (design.md "Libraries", the FFmpeg row;
//! "Threat Model" T02).
//!
//! FFmpeg reads a file only through an I/O context over the handle the
//! game handed the worker, never by a path or a URL: the context's
//! `io_open` refuses every open FFmpeg would make itself, the protocol
//! whitelist names no protocol, the format whitelist names the listed
//! containers and the codec whitelist the listed decoders, inherited by
//! every decoder FFmpeg opens while it probes. A video whose codec is not
//! listed, or whose frames exceed 4 Mi pixels or 8 192 on a side, is
//! refused at its probe, and its decoder keeps that cap for every later
//! frame. No single allocation FFmpeg makes exceeds 63 MiB, and each
//! decoder runs on at most two threads, so a healthy worker stays below
//! the line at which the game ends it. Frames come out as I420 at the
//! probed size, the size of the video's frame ring.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::ptr;

use ffmpeg_sys_next as ff;
use skin_model::media::control::{Failure, Refusal};
use skin_model::media::{MAX_PIXELS, MAX_SIDE};

/// The largest single allocation FFmpeg may make: below the 64 MiB margin
/// between the game's termination line and the job's cap
/// (design.md "Threat Model" T02).
pub const MAX_ALLOC: usize = 63 << 20;

/// The threads a decoder runs on, at most.
const THREADS: c_int = 2;

/// The bytes the I/O context reads at a time.
const IO_BUFFER: c_int = 1 << 16;

/// The listed containers, by FFmpeg's demuxer names (design.md
/// "Libraries", the FFmpeg row).
const FORMATS: &CStr = c"mpeg,mpegvideo,asf,mov,avi,matroska";

/// The listed decoders, by FFmpeg's decoder names (`msmpeg4` is MS-MPEG4
/// v3's).
const CODECS: &CStr = c"mpeg1video,mpeg2video,h264,hevc,vc1,wmv1,wmv2,wmv3,msmpeg4v2,msmpeg4,mpeg4,cinepak,msvideo1,vp8,mjpeg";

/// A protocol whitelist no protocol is on.
const PROTOCOLS: &CStr = c"none";

/// The listed codecs, by FFmpeg's ids: the worker refuses a video of any
/// other before a decoder opens.
const CODEC_IDS: [ff::AVCodecID; 15] = [
    ff::AVCodecID::AV_CODEC_ID_MPEG1VIDEO,
    ff::AVCodecID::AV_CODEC_ID_MPEG2VIDEO,
    ff::AVCodecID::AV_CODEC_ID_H264,
    ff::AVCodecID::AV_CODEC_ID_HEVC,
    ff::AVCodecID::AV_CODEC_ID_VC1,
    ff::AVCodecID::AV_CODEC_ID_WMV1,
    ff::AVCodecID::AV_CODEC_ID_WMV2,
    ff::AVCodecID::AV_CODEC_ID_WMV3,
    ff::AVCodecID::AV_CODEC_ID_MSMPEG4V2,
    ff::AVCodecID::AV_CODEC_ID_MSMPEG4V3,
    ff::AVCodecID::AV_CODEC_ID_MPEG4,
    ff::AVCodecID::AV_CODEC_ID_CINEPAK,
    ff::AVCodecID::AV_CODEC_ID_MSVIDEO1,
    ff::AVCodecID::AV_CODEC_ID_VP8,
    ff::AVCodecID::AV_CODEC_ID_MJPEG,
];

/// The furthest a seek starts before its target, in µs.
const MAX_BACK_OFF: i64 = 3_000_000;

/// Where each attempt of a seek starts, in µs before its target: at the
/// target, then a second further back each time the first frame decoded is
/// already past it.
const BACK_OFF: [i64; 4] = [0, 1_000_000, 2_000_000, MAX_BACK_OFF];

/// The pixel cap as FFmpeg's `max_pixels` option.
const MAX_PIXELS_OPTION: &CStr = c"4194304";

/// The ring's pixel format.
const I420: c_int = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int;

/// Sets FFmpeg up before any other call: the allocation cap ([`MAX_ALLOC`]
/// but in a dev-tools test) and the log.
pub fn init(max_alloc: usize) {
    // SAFETY: process-wide settings made before any other FFmpeg call and
    // before any thread that makes one starts.
    unsafe {
        ff::av_max_alloc(max_alloc);
        ff::av_log_set_level(ff::AV_LOG_WARNING);
        ff::av_log_set_callback(Some(log));
    }
}

/// FFmpeg's version string and configure line.
pub fn version() -> (String, String) {
    // SAFETY: both return static NUL-terminated strings.
    unsafe {
        (
            text(ff::av_version_info()),
            text(ff::avutil_configuration()),
        )
    }
}

/// The NUL-terminated string at `p`, or empty when `p` is null.
///
/// # Safety
///
/// `p` is null or points at a NUL-terminated string.
unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the caller's.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// FFmpeg's log, one line a message on standard error.
unsafe extern "C" fn log(avcl: *mut c_void, level: c_int, fmt: *const c_char, args: ff::va_list) {
    // SAFETY: a plain query.
    if level > unsafe { ff::av_log_get_level() } {
        return;
    }
    let mut line: [c_char; 1024] = [0; 1024];
    let mut prefix: c_int = 1;
    // SAFETY: FFmpeg's own arguments, passed on as they came; `line` holds
    // the 1024 bytes given, which the call NUL-terminates.
    unsafe {
        ff::av_log_format_line2(
            avcl,
            level,
            fmt,
            args,
            line.as_mut_ptr(),
            1024,
            &raw mut prefix,
        );
    }
    // SAFETY: NUL-terminated by the call above.
    let message = unsafe { CStr::from_ptr(line.as_ptr()) }.to_string_lossy();
    let message = message.trim_end();
    if !message.is_empty() {
        let _ = writeln!(std::io::stderr().lock(), "ffmpeg: {message}");
    }
}

/// Whether a frame of `width` × `height` is within the cap.
fn within_cap(width: c_int, height: c_int) -> bool {
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
        return false;
    };
    w > 0 && h > 0 && w <= MAX_SIDE && h <= MAX_SIDE && u64::from(w) * u64::from(h) <= MAX_PIXELS
}

/// FFmpeg's error for `errno`.
const fn averror(errno: c_int) -> c_int {
    ff::AVERROR(errno)
}

/// What a context's `io_open` does with FFmpeg's own opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoOpen {
    /// Refuses every one.
    Refuse,
    /// FFmpeg's own `io_open`: the control of the dev-tools probe.
    #[cfg(feature = "dev-tools")]
    Default,
}

/// Refuses an open FFmpeg would make itself, a playlist's or a reference's.
unsafe extern "C" fn refuse_open(
    _s: *mut ff::AVFormatContext,
    _pb: *mut *mut ff::AVIOContext,
    url: *const c_char,
    _flags: c_int,
    _options: *mut *mut ff::AVDictionary,
) -> c_int {
    // SAFETY: FFmpeg passes the URL it would open, NUL-terminated, or null.
    let url = unsafe { text(url) };
    let url: String = url.chars().take(256).collect();
    let _ = writeln!(
        std::io::stderr().lock(),
        "tapestry-media: refused a secondary open of {url:?}"
    );
    averror(ff::EPERM)
}

/// Reads the next bytes of the file for FFmpeg.
unsafe extern "C" fn read(opaque: *mut c_void, buf: *mut u8, size: c_int) -> c_int {
    let Ok(len) = usize::try_from(size) else {
        return averror(ff::EINVAL);
    };
    if opaque.is_null() || buf.is_null() {
        return averror(ff::EINVAL);
    }
    // SAFETY: `opaque` is the file this module boxed for this context,
    // alive until the context is freed, and FFmpeg hands a buffer of
    // `size` bytes.
    let (file, buf) = unsafe {
        (
            &mut *opaque.cast::<std::fs::File>(),
            std::slice::from_raw_parts_mut(buf, len),
        )
    };
    match file.read(buf) {
        Ok(0) => ff::AVERROR_EOF,
        Ok(n) => c_int::try_from(n).unwrap_or(size),
        Err(_) => averror(ff::EIO),
    }
}

/// Moves in the file for FFmpeg, or gives its size.
unsafe extern "C" fn seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    if opaque.is_null() {
        return i64::from(averror(ff::EINVAL));
    }
    // SAFETY: as in `read`.
    let file = unsafe { &mut *opaque.cast::<std::fs::File>() };
    let at = if whence & ff::AVSEEK_SIZE != 0 {
        size(file)
    } else {
        let to = match whence & !ff::AVSEEK_FORCE {
            0 => u64::try_from(offset).ok().map(SeekFrom::Start),
            1 => Some(SeekFrom::Current(offset)),
            2 => Some(SeekFrom::End(offset)),
            _ => None,
        };
        to.and_then(|to| file.seek(to).ok())
            .and_then(|at| i64::try_from(at).ok())
    };
    at.unwrap_or_else(|| i64::from(averror(ff::EIO)))
}

/// The file's length, its position kept.
fn size(file: &mut std::fs::File) -> Option<i64> {
    let here = file.stream_position().ok()?;
    let end = file.seek(SeekFrom::End(0)).ok()?;
    file.seek(SeekFrom::Start(here)).ok()?;
    i64::try_from(end).ok()
}

/// What a probe found.
#[derive(Clone, Debug)]
pub struct Probe {
    pub width: u32,
    pub height: u32,
    /// µs, 0 when unknown.
    pub duration: i64,
    pub codec: String,
    pub container: String,
}

/// Where a seek landed.
#[derive(Clone, Copy, Debug)]
pub struct Landed {
    /// The stamp of the first frame at or after the target, in the frame
    /// buffer.
    pub stamp: i64,
    pub attempts: u8,
}

/// A video file opened and probed, its decoder ready.
pub struct Input {
    fc: *mut ff::AVFormatContext,
    pb: *mut ff::AVIOContext,
    dec: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    scaled: *mut ff::AVFrame,
    sws: *mut ff::SwsContext,
    /// The file FFmpeg reads through `pb`: boxed so its address holds.
    file: Box<std::fs::File>,
    stream: c_int,
    time_base: ff::AVRational,
    start: i64,
    width: c_int,
    height: c_int,
    /// The demuxer has given its last packet and the decoder is draining.
    drained: bool,
}

impl Input {
    /// Opens and probes `file`: a listed container holding a video of a
    /// listed codec within the pixel cap, whose decoder opens.
    ///
    /// # Errors
    ///
    /// Why the file is refused, with FFmpeg's error (0 when there is
    /// none).
    pub fn open(file: std::fs::File, io: IoOpen) -> Result<(Self, Probe), (Refusal, c_int)> {
        let mut input = Self {
            fc: ptr::null_mut(),
            pb: ptr::null_mut(),
            dec: ptr::null_mut(),
            packet: ptr::null_mut(),
            frame: ptr::null_mut(),
            scaled: ptr::null_mut(),
            sws: ptr::null_mut(),
            file: Box::new(file),
            stream: -1,
            time_base: ff::AVRational { num: 0, den: 1 },
            start: 0,
            width: 0,
            height: 0,
            drained: false,
        };
        // SAFETY: each call gets the contexts this function allocated (or
        // FFmpeg's documented null), checked before use; `input` frees
        // whatever exists when it drops, on every return.
        let probe = unsafe { input.probe(io) }?;
        Ok((input, probe))
    }

    /// [`Input::open`]'s FFmpeg calls.
    ///
    /// # Safety
    ///
    /// Called once, on a fresh `Input`.
    unsafe fn probe(&mut self, io: IoOpen) -> Result<Probe, (Refusal, c_int)> {
        let no_memory = (Refusal::Decoder, averror(ff::ENOMEM));
        // SAFETY: the contexts are this input's, each checked
        // for null after it is allocated.
        unsafe {
            self.fc = ff::avformat_alloc_context();
            if self.fc.is_null() {
                return Err(no_memory);
            }
            let buf = ff::av_malloc(IO_BUFFER.unsigned_abs() as usize).cast::<u8>();
            if buf.is_null() {
                return Err(no_memory);
            }
            let opaque = ptr::from_mut::<std::fs::File>(&mut self.file).cast::<c_void>();
            self.pb =
                ff::avio_alloc_context(buf, IO_BUFFER, 0, opaque, Some(read), None, Some(seek));
            if self.pb.is_null() {
                ff::av_free(buf.cast());
                return Err(no_memory);
            }
            (*self.fc).pb = self.pb;
            (*self.fc).flags |= ff::AVFMT_FLAG_CUSTOM_IO;
            if io == IoOpen::Refuse {
                (*self.fc).io_open = Some(refuse_open);
            }
            let mut options = ptr::null_mut();
            ff::av_dict_set(
                &raw mut options,
                c"format_whitelist".as_ptr(),
                FORMATS.as_ptr(),
                0,
            );
            ff::av_dict_set(
                &raw mut options,
                c"protocol_whitelist".as_ptr(),
                PROTOCOLS.as_ptr(),
                0,
            );
            ff::av_dict_set(
                &raw mut options,
                c"codec_whitelist".as_ptr(),
                CODECS.as_ptr(),
                0,
            );
            let r = ff::avformat_open_input(
                &raw mut self.fc,
                c"".as_ptr(),
                ptr::null(),
                &raw mut options,
            );
            ff::av_dict_free(&raw mut options);
            if r < 0 {
                let why = if r == averror(ff::EIO) {
                    Refusal::Read
                } else {
                    Refusal::Container
                };
                return Err((why, r));
            }

            // The decoders FFmpeg opens while it probes keep the pixel cap
            // too (and the codec whitelist, which it passes on itself). It
            // gives the per-stream options only to the streams that exist
            // when its probe starts, and a demuxer without a header (MPEG-PS)
            // adds streams as it reads them: such a file is read first as far
            // as the probe reads, so every stream the probe can meet exists,
            // and then from its first byte again.
            if (*self.fc).ctx_flags & ff::AVFMTCTX_NOHEADER != 0 {
                self.find_streams()?;
            }
            let streams = (*self.fc).nb_streams as usize;
            let mut per_stream: Vec<*mut ff::AVDictionary> = vec![ptr::null_mut(); streams];
            for d in &mut per_stream {
                ff::av_dict_set(d, c"max_pixels".as_ptr(), MAX_PIXELS_OPTION.as_ptr(), 0);
            }
            let r = ff::avformat_find_stream_info(self.fc, per_stream.as_mut_ptr());
            for d in &mut per_stream {
                ff::av_dict_free(d);
            }
            if r < 0 {
                return Err((Refusal::Container, r));
            }
            let s = ff::av_find_best_stream(
                self.fc,
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                ptr::null_mut(),
                0,
            );
            if s < 0 {
                return Err((Refusal::NoVideo, s));
            }
            let st = *(*self.fc).streams.add(s.unsigned_abs() as usize);
            let par = (*st).codecpar;
            let id = (*par).codec_id;
            if !CODEC_IDS.contains(&id) {
                return Err((Refusal::Codec, 0));
            }
            let (width, height) = ((*par).width, (*par).height);
            if !within_cap(width, height) {
                return Err((Refusal::Size, 0));
            }
            let decoder = ff::avcodec_find_decoder(id);
            if decoder.is_null() {
                return Err((Refusal::Decoder, ff::AVERROR_DECODER_NOT_FOUND));
            }
            self.dec = ff::avcodec_alloc_context3(decoder);
            if self.dec.is_null() {
                return Err(no_memory);
            }
            let r = ff::avcodec_parameters_to_context(self.dec, par);
            if r < 0 {
                return Err((Refusal::Decoder, r));
            }
            (*self.dec).thread_count = THREADS;
            (*self.dec).max_pixels = MAX_PIXELS.cast_signed();
            (*self.dec).pkt_timebase = (*st).time_base;
            let mut options = ptr::null_mut();
            ff::av_dict_set(
                &raw mut options,
                c"codec_whitelist".as_ptr(),
                CODECS.as_ptr(),
                0,
            );
            let r = ff::avcodec_open2(self.dec, decoder, &raw mut options);
            ff::av_dict_free(&raw mut options);
            if r < 0 {
                return Err((Refusal::Decoder, r));
            }
            self.packet = ff::av_packet_alloc();
            self.frame = ff::av_frame_alloc();
            self.scaled = ff::av_frame_alloc();
            if self.packet.is_null() || self.frame.is_null() || self.scaled.is_null() {
                return Err(no_memory);
            }
            (*self.scaled).format = I420;
            (*self.scaled).width = width;
            (*self.scaled).height = height;
            let r = ff::av_frame_get_buffer(self.scaled, 0);
            if r < 0 {
                return Err((Refusal::Decoder, r));
            }
            self.stream = s;
            self.time_base = (*st).time_base;
            self.start = if (*st).start_time == ff::AV_NOPTS_VALUE {
                0
            } else {
                (*st).start_time
            };
            self.width = width;
            self.height = height;
            let duration = (*self.fc).duration;
            Ok(Probe {
                width: width.unsigned_abs(),
                height: height.unsigned_abs(),
                duration: if duration > 0 { duration } else { 0 },
                codec: text(ff::avcodec_get_name(id)),
                container: text((*(*self.fc).iformat).name),
            })
        }
    }

    /// Reads the packets of the probe's budget (`probesize` bytes, all the
    /// probe reads), which creates every stream a headerless demuxer meets
    /// in them, and returns to the file's first byte. Nothing is decoded.
    ///
    /// # Safety
    ///
    /// The format context is open.
    unsafe fn find_streams(&mut self) -> Result<(), (Refusal, c_int)> {
        // SAFETY: this input's open context and a packet of its own, freed
        // before returning.
        unsafe {
            let mut packet = ff::av_packet_alloc();
            if packet.is_null() {
                return Err((Refusal::Decoder, averror(ff::ENOMEM)));
            }
            let budget = (*self.fc).probesize;
            let mut read: i64 = 0;
            while read < budget && ff::av_read_frame(self.fc, packet) >= 0 {
                read = read.saturating_add(i64::from((*packet).size));
                ff::av_packet_unref(packet);
            }
            ff::av_packet_free(&raw mut packet);
            let r = ff::av_seek_frame(self.fc, -1, 0, ff::AVSEEK_FLAG_BYTE);
            if r < 0 {
                return Err((Refusal::Container, r));
            }
        }
        Ok(())
    }

    /// Decodes the next frame into `out`, I420 at the probed size: its
    /// stamp, in µs from the stream's start, or `None` at the end. A frame
    /// with no time is decoded but not returned.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, an allocation is refused or the
    /// decoder fails; a damaged packet FFmpeg skips is not an error.
    pub fn next(&mut self, out: &mut [u8]) -> Result<Option<i64>, (Failure, c_int)> {
        loop {
            if !self.decode()? {
                return Ok(None);
            }
            if let Some(stamp) = self.stamp() {
                self.convert(out)?;
                return Ok(Some(stamp));
            }
        }
    }

    /// Positions the video so its first frame at or after `target` µs is
    /// decoded into `out`: seeks to the target, and when the first frame
    /// decoded there is already past it (the demuxer overshot), seeks again
    /// from a second further back, up to three times. `None` when no frame
    /// at or after the target exists.
    ///
    /// # Errors
    ///
    /// As [`Input::next`], or when the demuxer cannot seek.
    pub fn seek(
        &mut self,
        target: i64,
        out: &mut [u8],
    ) -> Result<Option<Landed>, (Failure, c_int)> {
        for (attempts, back) in (1..).zip(BACK_OFF) {
            let to = target.saturating_sub(back);
            // SAFETY: this input's contexts.
            let r = unsafe {
                let ts = ff::av_rescale_q(to, ff::AV_TIME_BASE_Q, self.time_base)
                    .saturating_add(self.start);
                let r = ff::avformat_seek_file(self.fc, self.stream, i64::MIN, ts, ts, 0);
                ff::avcodec_flush_buffers(self.dec);
                r
            };
            if r < 0 {
                return Err((Failure::Seek, r));
            }
            self.drained = false;
            let mut first = None;
            let landed = loop {
                if !self.decode()? {
                    break None;
                }
                let Some(stamp) = self.stamp() else {
                    continue;
                };
                first.get_or_insert(stamp);
                if stamp >= target {
                    break Some(stamp);
                }
            };
            let exact = first.is_some_and(|f| f <= target);
            if exact || back == MAX_BACK_OFF || landed.is_none() {
                return match landed {
                    Some(stamp) => {
                        self.convert(out)?;
                        Ok(Some(Landed { stamp, attempts }))
                    }
                    None => Ok(None),
                };
            }
        }
        Ok(None)
    }

    /// Decodes the next frame into `self.frame`: whether there was one.
    fn decode(&mut self) -> Result<bool, (Failure, c_int)> {
        loop {
            // SAFETY: this input's contexts.
            unsafe {
                let r = ff::avcodec_receive_frame(self.dec, self.frame);
                if r >= 0 {
                    return Ok(true);
                }
                if r == ff::AVERROR_EOF {
                    return Ok(false);
                }
                if r == averror(ff::ENOMEM) {
                    return Err((Failure::Memory, r));
                }
                if r != averror(ff::EAGAIN) && r != ff::AVERROR_INVALIDDATA {
                    return Err((Failure::Decode, r));
                }
                if self.drained {
                    return Ok(false);
                }
                let r = ff::av_read_frame(self.fc, self.packet);
                if r == averror(ff::ENOMEM) {
                    return Err((Failure::Memory, r));
                }
                if r == averror(ff::EIO) {
                    return Err((Failure::Read, r));
                }
                if r < 0 {
                    // The end of the file, or a demuxing error, which ends
                    // the video as FFmpeg's own command line ends its input
                    // on one: the decoder gives the frames it still holds.
                    if r != ff::AVERROR_EOF {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "tapestry-media: demuxing ended on FFmpeg error {r}"
                        );
                    }
                    self.drained = true;
                    let r = ff::avcodec_send_packet(self.dec, ptr::null());
                    if r < 0 && r != ff::AVERROR_EOF {
                        return Err((Failure::Decode, r));
                    }
                    continue;
                }
                if (*self.packet).stream_index != self.stream {
                    ff::av_packet_unref(self.packet);
                    continue;
                }
                let r = ff::avcodec_send_packet(self.dec, self.packet);
                ff::av_packet_unref(self.packet);
                if r == averror(ff::ENOMEM) {
                    return Err((Failure::Memory, r));
                }
                if r < 0 && r != ff::AVERROR_INVALIDDATA && r != averror(ff::EAGAIN) {
                    return Err((Failure::Decode, r));
                }
            }
        }
    }

    /// The decoded frame's stamp in µs from the stream's start, or `None`
    /// when it has no time.
    fn stamp(&self) -> Option<i64> {
        // SAFETY: this input's frame and plain arithmetic.
        unsafe {
            let pts = (*self.frame).best_effort_timestamp;
            if pts == ff::AV_NOPTS_VALUE {
                return None;
            }
            let from_start = pts.checked_sub(self.start)?;
            Some(ff::av_rescale_q(
                from_start,
                self.time_base,
                ff::AV_TIME_BASE_Q,
            ))
        }
    }

    /// The decoded frame into `out`, I420 at the probed size, scaled when
    /// its size or format differs.
    fn convert(&mut self, out: &mut [u8]) -> Result<(), (Failure, c_int)> {
        // SAFETY: this input's frames and scaler.
        unsafe {
            let f = self.frame;
            let src =
                if (*f).format == I420 && (*f).width == self.width && (*f).height == self.height {
                    f.cast_const()
                } else {
                    if self.sws.is_null() {
                        self.sws = ff::sws_alloc_context();
                        if self.sws.is_null() {
                            return Err((Failure::Memory, averror(ff::ENOMEM)));
                        }
                        (*self.sws).threads = 1;
                        (*self.sws).flags = ff::SwsFlags::SWS_BILINEAR as u32;
                    }
                    let r = ff::sws_scale_frame(self.sws, self.scaled, f);
                    if r < 0 {
                        return Err((Failure::Decode, r));
                    }
                    self.scaled.cast_const()
                };
            if copy_i420(src, self.width, self.height, out) {
                Ok(())
            } else {
                Err((Failure::Decode, 0))
            }
        }
    }

    /// Asks the context's `io_open` for `url`, as a playlist or reference
    /// would: FFmpeg's error, or 0 when it opened.
    #[cfg(feature = "dev-tools")]
    pub fn secondary_open(&mut self, url: &CStr) -> c_int {
        // SAFETY: this input's context; a context it opened is closed
        // through the same context.
        unsafe {
            let Some(open) = (*self.fc).io_open else {
                return averror(ff::ENOSYS);
            };
            let mut pb = ptr::null_mut();
            let r = open(
                self.fc,
                &raw mut pb,
                url.as_ptr(),
                ff::AVIO_FLAG_READ,
                ptr::null_mut(),
            );
            if r >= 0
                && !pb.is_null()
                && let Some(close) = (*self.fc).io_close2
            {
                close(self.fc, pb);
            }
            r
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: each is this input's, freed once; FFmpeg's free functions
        // take null. The custom I/O context and its buffer are not freed by
        // `avformat_close_input`.
        unsafe {
            ff::sws_free_context(&raw mut self.sws);
            ff::av_frame_free(&raw mut self.scaled);
            ff::av_frame_free(&raw mut self.frame);
            ff::av_packet_free(&raw mut self.packet);
            ff::avcodec_free_context(&raw mut self.dec);
            ff::avformat_close_input(&raw mut self.fc);
            if !self.pb.is_null() {
                ff::av_freep((&raw mut (*self.pb).buffer).cast());
                ff::avio_context_free(&raw mut self.pb);
            }
        }
    }
}

/// Copies an I420 frame of `width` × `height` into `out`, its planes one
/// after another with no padding.
///
/// # Safety
///
/// `frame` is a decoded or scaled frame whose planes hold its size.
unsafe fn copy_i420(
    frame: *const ff::AVFrame,
    width: c_int,
    height: c_int,
    out: &mut [u8],
) -> bool {
    let (w, h) = (
        width.unsigned_abs() as usize,
        height.unsigned_abs() as usize,
    );
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut at = 0;
    for (plane, pw, ph) in [(0, w, h), (1, cw, ch), (2, cw, ch)] {
        // SAFETY: the caller's frame.
        let (data, stride) = unsafe { ((*frame).data[plane], (*frame).linesize[plane]) };
        let Ok(stride) = usize::try_from(stride) else {
            return false;
        };
        if data.is_null() || stride < pw {
            return false;
        }
        for row in 0..ph {
            let Some(dst) = out.get_mut(at..at + pw) else {
                return false;
            };
            // SAFETY: the plane holds `ph` rows of `stride` bytes, each
            // with `pw` bytes of pixels.
            let src = unsafe { std::slice::from_raw_parts(data.add(row * stride), pw) };
            dst.copy_from_slice(src);
            at += pw;
        }
    }
    at == out.len()
}

/// A synthesized clip for the tests and the reference rig (dev-tools).
#[cfg(feature = "dev-tools")]
pub struct Clip {
    /// FFmpeg's encoder name.
    pub codec: String,
    /// FFmpeg's muxer name.
    pub format: String,
    pub width: c_int,
    pub height: c_int,
    /// Frames a second.
    pub rate: c_int,
    pub frames: u32,
    /// Frames between key frames.
    pub gop: c_int,
}

/// Writes `clip` to `path` through the build's encoder and muxer, frame `i`
/// painted by `paint(i, i420)` into a tight I420 frame of the clip's size.
/// The file is written through an I/O context of the worker's own, since
/// the build has no file protocol.
///
/// # Errors
///
/// What failed, as text.
#[cfg(feature = "dev-tools")]
pub fn encode_clip(
    path: &std::path::Path,
    clip: &Clip,
    paint: &mut dyn FnMut(u32, &mut [u8]),
) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = Encoding {
        oc: ptr::null_mut(),
        pb: ptr::null_mut(),
        ctx: ptr::null_mut(),
        src: ptr::null_mut(),
        conv: ptr::null_mut(),
        sws: ptr::null_mut(),
        packet: ptr::null_mut(),
        file: Box::new(file),
    };
    // SAFETY: `enc` owns every context the encoding allocates and frees
    // them when it drops.
    unsafe { enc.run(clip, paint) }
}

/// The contexts of one [`encode_clip`].
#[cfg(feature = "dev-tools")]
struct Encoding {
    oc: *mut ff::AVFormatContext,
    pb: *mut ff::AVIOContext,
    ctx: *mut ff::AVCodecContext,
    src: *mut ff::AVFrame,
    conv: *mut ff::AVFrame,
    sws: *mut ff::SwsContext,
    packet: *mut ff::AVPacket,
    file: Box<std::fs::File>,
}

#[cfg(feature = "dev-tools")]
impl Encoding {
    /// # Safety
    ///
    /// Called once, on a fresh `Encoding`.
    unsafe fn run(
        &mut self,
        clip: &Clip,
        paint: &mut dyn FnMut(u32, &mut [u8]),
    ) -> Result<(), String> {
        let fail = |what: &str, r: c_int| format!("{what}: FFmpeg error {r}");
        let codec = std::ffi::CString::new(clip.codec.as_str()).map_err(|e| e.to_string())?;
        let format = std::ffi::CString::new(clip.format.as_str()).map_err(|e| e.to_string())?;
        // SAFETY: the contexts are this encoding's, each
        // checked for null after it is allocated.
        unsafe {
            let encoder = ff::avcodec_find_encoder_by_name(codec.as_ptr());
            if encoder.is_null() {
                return Err(format!("no encoder {}", clip.codec));
            }
            let r = ff::avformat_alloc_output_context2(
                &raw mut self.oc,
                ptr::null(),
                format.as_ptr(),
                ptr::null(),
            );
            if r < 0 || self.oc.is_null() {
                return Err(fail("no muxer", r));
            }
            let st = ff::avformat_new_stream(self.oc, ptr::null());
            self.ctx = ff::avcodec_alloc_context3(encoder);
            self.packet = ff::av_packet_alloc();
            self.src = ff::av_frame_alloc();
            if st.is_null() || self.ctx.is_null() || self.packet.is_null() || self.src.is_null() {
                return Err("out of memory".to_owned());
            }
            let ctx = self.ctx;
            (*ctx).width = clip.width;
            (*ctx).height = clip.height;
            (*ctx).time_base = ff::AVRational {
                num: 1,
                den: clip.rate,
            };
            (*ctx).framerate = ff::AVRational {
                num: clip.rate,
                den: 1,
            };
            (*ctx).gop_size = clip.gop;
            (*ctx).max_b_frames = 0;
            (*ctx).thread_count = 1;
            (*ctx).pix_fmt = pixel_format(ctx, encoder);
            if clip.codec == "mjpeg" {
                // JPEG's own range, which the MJPEG encoder requires.
                (*ctx).color_range = ff::AVColorRange::AVCOL_RANGE_JPEG;
            }
            (*ctx).flags |= ff::AV_CODEC_FLAG_QSCALE.cast_signed();
            (*ctx).global_quality = ff::FF_QP2LAMBDA * 3;
            if (*(*self.oc).oformat).flags & ff::AVFMT_GLOBALHEADER != 0 {
                (*ctx).flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER.cast_signed();
            }
            let r = ff::avcodec_open2(ctx, encoder, ptr::null_mut());
            if r < 0 {
                return Err(fail("encoder", r));
            }
            let r = ff::avcodec_parameters_from_context((*st).codecpar, ctx);
            if r < 0 {
                return Err(fail("parameters", r));
            }
            (*st).time_base = (*ctx).time_base;

            let buf = ff::av_malloc(IO_BUFFER.unsigned_abs() as usize).cast::<u8>();
            if buf.is_null() {
                return Err("out of memory".to_owned());
            }
            let opaque = ptr::from_mut::<std::fs::File>(&mut self.file).cast::<c_void>();
            self.pb =
                ff::avio_alloc_context(buf, IO_BUFFER, 1, opaque, None, Some(write), Some(seek));
            if self.pb.is_null() {
                ff::av_free(buf.cast());
                return Err("out of memory".to_owned());
            }
            (*self.oc).pb = self.pb;
            let r = ff::avformat_write_header(self.oc, ptr::null_mut());
            if r < 0 {
                return Err(fail("header", r));
            }

            (*self.src).format = I420;
            (*self.src).width = clip.width;
            (*self.src).height = clip.height;
            if ff::av_frame_get_buffer(self.src, 0) < 0 {
                return Err("out of memory".to_owned());
            }
            if (*ctx).pix_fmt as c_int != I420 {
                self.conv = ff::av_frame_alloc();
                self.sws = ff::sws_alloc_context();
                if self.conv.is_null() || self.sws.is_null() {
                    return Err("out of memory".to_owned());
                }
                (*self.conv).format = (*ctx).pix_fmt as c_int;
                (*self.conv).width = clip.width;
                (*self.conv).height = clip.height;
                if ff::av_frame_get_buffer(self.conv, 0) < 0 {
                    return Err("out of memory".to_owned());
                }
                (*self.sws).threads = 1;
            }
            let (w, h) = (
                clip.width.unsigned_abs() as usize,
                clip.height.unsigned_abs() as usize,
            );
            let mut tight = vec![0_u8; w * h + 2 * w.div_ceil(2) * h.div_ceil(2)];
            for i in 0..clip.frames {
                paint(i, &mut tight);
                if ff::av_frame_make_writable(self.src) < 0 {
                    return Err("frame not writable".to_owned());
                }
                fill_i420(self.src, w, h, &tight);
                let frame = if self.conv.is_null() {
                    self.src
                } else {
                    if ff::av_frame_make_writable(self.conv) < 0 {
                        return Err("frame not writable".to_owned());
                    }
                    let r = ff::sws_scale_frame(self.sws, self.conv, self.src);
                    if r < 0 {
                        return Err(fail("scale", r));
                    }
                    self.conv
                };
                (*frame).pts = i64::from(i);
                (*frame).quality = (*ctx).global_quality;
                let r = ff::avcodec_send_frame(ctx, frame);
                if r < 0 {
                    return Err(fail("encode", r));
                }
                self.drain(st)?;
            }
            let r = ff::avcodec_send_frame(ctx, ptr::null());
            if r < 0 {
                return Err(fail("flush", r));
            }
            self.drain(st)?;
            let r = ff::av_write_trailer(self.oc);
            if r < 0 {
                return Err(fail("trailer", r));
            }
            ff::avio_flush(self.pb);
        }
        self.file.flush().map_err(|e| e.to_string())
    }

    /// Writes every packet the encoder has ready.
    ///
    /// # Safety
    ///
    /// `st` is this encoding's stream.
    unsafe fn drain(&mut self, st: *mut ff::AVStream) -> Result<(), String> {
        loop {
            // SAFETY: this encoding's contexts and the caller's stream.
            unsafe {
                let r = ff::avcodec_receive_packet(self.ctx, self.packet);
                if r == averror(ff::EAGAIN) || r == ff::AVERROR_EOF {
                    return Ok(());
                }
                if r < 0 {
                    return Err(format!("encode: FFmpeg error {r}"));
                }
                ff::av_packet_rescale_ts(self.packet, (*self.ctx).time_base, (*st).time_base);
                (*self.packet).stream_index = (*st).index;
                let r = ff::av_interleaved_write_frame(self.oc, self.packet);
                if r < 0 {
                    return Err(format!("mux: FFmpeg error {r}"));
                }
            }
        }
    }
}

#[cfg(feature = "dev-tools")]
impl Drop for Encoding {
    fn drop(&mut self) {
        // SAFETY: each is this encoding's, freed once; FFmpeg's free
        // functions take null. The output context's I/O is ours to free.
        unsafe {
            ff::sws_free_context(&raw mut self.sws);
            ff::av_frame_free(&raw mut self.conv);
            ff::av_frame_free(&raw mut self.src);
            ff::av_packet_free(&raw mut self.packet);
            ff::avcodec_free_context(&raw mut self.ctx);
            if !self.oc.is_null() {
                (*self.oc).pb = ptr::null_mut();
                ff::avformat_free_context(self.oc);
                self.oc = ptr::null_mut();
            }
            if !self.pb.is_null() {
                ff::av_freep((&raw mut (*self.pb).buffer).cast());
                ff::avio_context_free(&raw mut self.pb);
            }
        }
    }
}

/// The encoder's pixel format: I420 when it takes it, else its first.
///
/// # Safety
///
/// `ctx` is an allocated context of `encoder`.
#[cfg(feature = "dev-tools")]
unsafe fn pixel_format(
    ctx: *const ff::AVCodecContext,
    encoder: *const ff::AVCodec,
) -> ff::AVPixelFormat {
    let mut configs: *const c_void = ptr::null();
    let mut n: c_int = 0;
    // SAFETY: the caller's; FFmpeg returns an array of `n` pixel formats,
    // or null for all.
    unsafe {
        let r = ff::avcodec_get_supported_config(
            ctx,
            encoder,
            ff::AVCodecConfig::AV_CODEC_CONFIG_PIX_FORMAT,
            0,
            &raw mut configs,
            &raw mut n,
        );
        if r < 0 || configs.is_null() || n <= 0 {
            return ff::AVPixelFormat::AV_PIX_FMT_YUV420P;
        }
        let formats = std::slice::from_raw_parts(
            configs.cast::<ff::AVPixelFormat>(),
            n.unsigned_abs() as usize,
        );
        if formats.contains(&ff::AVPixelFormat::AV_PIX_FMT_YUV420P) {
            ff::AVPixelFormat::AV_PIX_FMT_YUV420P
        } else {
            formats
                .first()
                .copied()
                .unwrap_or(ff::AVPixelFormat::AV_PIX_FMT_YUV420P)
        }
    }
}

/// Copies a tight I420 frame into `frame`'s planes.
///
/// # Safety
///
/// `frame` is a writable I420 frame of `w` × `h`.
#[cfg(feature = "dev-tools")]
unsafe fn fill_i420(frame: *mut ff::AVFrame, w: usize, h: usize, tight: &[u8]) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut at = 0;
    for (plane, pw, ph) in [(0, w, h), (1, cw, ch), (2, cw, ch)] {
        // SAFETY: the caller's frame, whose planes hold `ph` rows of
        // `linesize` bytes.
        unsafe {
            let data = (*frame).data[plane];
            let stride = (*frame).linesize[plane].unsigned_abs() as usize;
            for row in 0..ph {
                if let Some(src) = tight.get(at..at + pw) {
                    std::ptr::copy_nonoverlapping(src.as_ptr(), data.add(row * stride), pw);
                }
                at += pw;
            }
        }
    }
}

/// Writes bytes FFmpeg muxed to the clip's file.
#[cfg(feature = "dev-tools")]
unsafe extern "C" fn write(opaque: *mut c_void, buf: *const u8, size: c_int) -> c_int {
    let Ok(len) = usize::try_from(size) else {
        return averror(ff::EINVAL);
    };
    if opaque.is_null() || buf.is_null() {
        return averror(ff::EINVAL);
    }
    // SAFETY: `opaque` is the file this module boxed for this context, and
    // FFmpeg hands `size` bytes.
    let (file, buf) = unsafe {
        (
            &mut *opaque.cast::<std::fs::File>(),
            std::slice::from_raw_parts(buf, len),
        )
    };
    match file.write_all(buf) {
        Ok(()) => size,
        Err(_) => averror(ff::EIO),
    }
}
