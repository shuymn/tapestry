//! Every call the worker makes into FFmpeg: the one module that holds the
//! worker's unsafe FFmpeg code (design.md "Libraries", the FFmpeg row;
//! "Threat Model" T02).
//!
//! FFmpeg reads a file only through an I/O context over the handle the
//! game handed the worker, never by a path or a URL: the context's
//! `io_open` refuses every open FFmpeg would make itself, the protocol
//! whitelist names no protocol, the format whitelist names the listed
//! containers and the codec whitelist the listed decoders. While a file is
//! probed, and while it seeks, the context charges each byte read and each
//! seek to a budget, and refuses both once the budget is spent.
//!
//! Before anything else runs, the worker points FFmpeg's DLLs at hooks of
//! its own. Every allocation they ask their C runtime for passes the
//! worker's allocator, which refuses one above 63 MiB and records each
//! refusal, its own or the runtime's, for the player the asking thread
//! works for. Every thread they start passes the worker's start, which has
//! the new thread work for the player of the thread that started it, and
//! DLLs that import any other way to start a thread are refused. Every
//! decoder the format library opens while it probes passes the worker's
//! open, which refuses a codec off the list and gives the decoder the pixel
//! cap and the worker's frame allocation, as the worker gives its own
//! decoder. That allocation refuses a frame whose coded size, the size
//! FFmpeg allocates, is over 4 Mi pixels or 8 192 on a side, before
//! anything is allocated, and records that too. So every refusal made for
//! a player, on its own thread or on one FFmpeg started for it, is recorded
//! for that player: a probe that met one refuses the file, and a player
//! fails on one before it takes another frame or the video's end, even
//! where FFmpeg went on as if the frame were damaged or the file had ended.
//! Each decoder runs on at most two threads, so a healthy worker stays
//! below the line at which the game ends it. Frames come out as I420 at
//! the probed size, the size of the video's frame ring.

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

/// The bytes a probe or a seek may read, and the seeks it may ask for, a
/// request for the file's size counted as one: well over what FFmpeg reads
/// and seeks to probe a healthy video or to seek in one, whatever a file's
/// layout makes it scan.
const BUDGET: (u64, u32) = (64 << 20, 256);

/// FFmpeg's error for a read or a seek past its budget.
pub const SPENT: c_int = averror(ff::EFBIG);

/// The threads a decoder runs on, at most, so a healthy worker stays below
/// the line at which the game ends it. FFmpeg starts them through the
/// worker's start, so each records its refusals for the decoder's player.
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

/// FFmpeg's codec whitelist is left unset at every open (dev-tools): what
/// an allocation refused while it is set would leave.
#[cfg(feature = "dev-tools")]
static NO_CODEC_LIST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Leaves FFmpeg's codec whitelist unset at every open from now on
/// (dev-tools).
#[cfg(feature = "dev-tools")]
pub fn leave_codec_list_unset() {
    NO_CODEC_LIST.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The codec whitelist, as the option an open sets.
fn codec_list() -> &'static [(&'static CStr, &'static CStr)] {
    #[cfg(feature = "dev-tools")]
    if NO_CODEC_LIST.load(std::sync::atomic::Ordering::Relaxed) {
        return &[];
    }
    &[(c"codec_whitelist", CODECS)]
}

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

/// The damaged frames a decoder may give among those it holds when the
/// file ends, more than any decoder holds: past them the video ends.
const DAMAGED_AT_END: u32 = 64;

/// The furthest a seek starts before its target, in µs.
const MAX_BACK_OFF: i64 = 3_000_000;

/// Where each attempt of a seek starts, in µs before its target: at the
/// target, then a second further back each time the first frame decoded is
/// already past it.
const BACK_OFF: [i64; 4] = [0, 1_000_000, 2_000_000, MAX_BACK_OFF];

/// The ring's pixel format.
const I420: c_int = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int;

/// Sets FFmpeg up before any other call: the worker's hooks, with the
/// allocation cap at `max_alloc` ([`MAX_ALLOC`] but in a dev-tools test),
/// and the log.
///
/// # Errors
///
/// When a DLL's imports cannot be pointed at the hooks: the worker then
/// serves nothing.
pub fn init(max_alloc: usize) -> Result<(), String> {
    hooks::install(max_alloc)?;
    // SAFETY: process-wide settings made before any other FFmpeg call and
    // before any thread that makes one starts.
    unsafe {
        ff::av_log_set_level(ff::AV_LOG_WARNING);
        ff::av_log_set_callback(Some(log));
    }
    Ok(())
}

/// The worker's hooks in FFmpeg's DLLs, and what they record.
mod hooks {
    use std::alloc::Layout;
    use std::cell::Cell;
    use std::ffi::{CStr, c_int, c_void};
    use std::io::Write as _;
    use std::ptr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::{CODEC_IDS, MAX_PIXELS, averror, ff, text, within_cap};

    type Malloc = unsafe extern "C" fn(usize) -> *mut c_void;
    type Calloc = unsafe extern "C" fn(usize, usize) -> *mut c_void;
    type Realloc = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
    type AlignedMalloc = unsafe extern "C" fn(usize, usize) -> *mut c_void;
    type AlignedRealloc = unsafe extern "C" fn(*mut c_void, usize, usize) -> *mut c_void;
    /// A thread's start routine, as `_beginthreadex` takes it.
    type Start = unsafe extern "system" fn(*mut c_void) -> u32;
    type BeginThread =
        unsafe extern "C" fn(*mut c_void, u32, Option<Start>, *mut c_void, u32, *mut u32) -> usize;
    type Open = unsafe extern "C" fn(
        *mut ff::AVCodecContext,
        *const ff::AVCodec,
        *mut *mut ff::AVDictionary,
    ) -> c_int;

    /// The largest single allocation the worker's allocator makes.
    static CAP: AtomicUsize = AtomicUsize::new(super::MAX_ALLOC);

    // The functions the DLLs' imports held before the worker pointed them
    // at its hooks, 0 until then: the C runtime's allocators and thread
    // start, and the codec library's `avcodec_open2` as the format library
    // imports it.
    static MALLOC: AtomicUsize = AtomicUsize::new(0);
    static CALLOC: AtomicUsize = AtomicUsize::new(0);
    static REALLOC: AtomicUsize = AtomicUsize::new(0);
    static ALIGNED_MALLOC: AtomicUsize = AtomicUsize::new(0);
    static ALIGNED_REALLOC: AtomicUsize = AtomicUsize::new(0);
    static BEGIN_THREAD: AtomicUsize = AtomicUsize::new(0);
    static OPEN: AtomicUsize = AtomicUsize::new(0);

    /// The functions besides `_beginthreadex` that start a thread, or run
    /// code on a thread of the system's: one FFmpeg started so would work
    /// for no player, so FFmpeg's DLLs may import none of them.
    const OTHER_STARTS: [&CStr; 14] = [
        c"CreateThread",
        c"CreateRemoteThread",
        c"CreateRemoteThreadEx",
        c"_beginthread",
        c"thrd_create",
        c"QueueUserWorkItem",
        c"TrySubmitThreadpoolCallback",
        c"CreateThreadpoolWork",
        c"CreateThreadpoolTimer",
        c"CreateThreadpoolWait",
        c"CreateThreadpoolIo",
        c"CreateTimerQueueTimer",
        c"RegisterWaitForSingleObject",
        c"BindIoCompletionCallback",
    ];

    /// What the threads working for a player record for it until it looks.
    #[derive(Default)]
    pub struct Record {
        /// The largest allocation refused, 0 when none was.
        refused: AtomicUsize,
        /// A frame over the cap was refused.
        oversize: AtomicBool,
    }

    impl Record {
        /// The largest allocation refused since the last look, forgotten
        /// now.
        pub fn refused(&self) -> Option<usize> {
            Some(self.refused.swap(0, Ordering::SeqCst)).filter(|&size| size > 0)
        }

        /// Whether a frame over the cap was refused since the last look,
        /// forgotten now.
        pub fn oversize(&self) -> bool {
            self.oversize.swap(false, Ordering::SeqCst)
        }
    }

    /// What a thread working for no player records, which nobody reads:
    /// the worker's own setup's, and its dev-tools encoder's.
    static NOBODY: Record = Record {
        refused: AtomicUsize::new(0),
        oversize: AtomicBool::new(false),
    };

    thread_local! {
        /// The record of the player this thread works for, null when it
        /// works for none: a player's input sets its own ([`Owner`]), and a
        /// thread FFmpeg starts gets the one of the thread that started it.
        /// A record here holds one count of its `Arc` (`Arc::into_raw`).
        static CURRENT: Cell<*const Record> = const { Cell::new(ptr::null()) };
    }

    /// Runs `f` on the record of the player this thread works for.
    fn record(f: impl FnOnce(&Record)) {
        let current = CURRENT.with(Cell::get);
        if current.is_null() {
            f(&NOBODY);
        } else {
            // SAFETY: the count this thread's `CURRENT` holds keeps it
            // alive while it is there.
            f(unsafe { &*current });
        }
    }

    /// A player's record, which its thread and every thread FFmpeg starts
    /// from it, or from those, record for until it drops.
    pub struct Owner {
        /// The record, whose count this thread's `CURRENT` holds.
        record: *const Record,
        /// What `CURRENT` held before.
        previous: *const Record,
    }

    impl Owner {
        /// Has this thread record for a fresh record of its own.
        pub fn adopt() -> Self {
            let record = Arc::into_raw(Arc::new(Record::default()));
            let previous = CURRENT.with(|c| c.replace(record));
            Self { record, previous }
        }

        pub fn record(&self) -> &Record {
            // SAFETY: the count `CURRENT` holds, given back only when this
            // owner drops.
            unsafe { &*self.record }
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            // The record is given back only while this thread still records
            // for it: an owner dropped before one adopted after it stays
            // what the later one gives `CURRENT` back to, so it is kept.
            let restored = CURRENT.with(|c| {
                let ours = c.get() == self.record;
                if ours {
                    c.set(self.previous);
                }
                ours
            });
            if restored {
                // SAFETY: the count `CURRENT` held, which it no longer does.
                unsafe { drop(Arc::from_raw(self.record)) };
            }
        }
    }

    /// Records an allocation of `size` bytes refused for the player this
    /// thread works for.
    fn refuse(size: usize) {
        record(|r| {
            r.refused.fetch_max(size, Ordering::SeqCst);
        });
    }

    /// Whether `size` bytes may be allocated at once; a larger allocation is
    /// refused and recorded.
    fn allowed(size: usize) -> bool {
        if size > CAP.load(Ordering::Relaxed) {
            refuse(size.max(1));
            return false;
        }
        true
    }

    /// The function an import held before the worker's hook, or `None`
    /// before the hook is installed.
    fn held(original: &AtomicUsize) -> Option<usize> {
        Some(original.load(Ordering::Relaxed)).filter(|&f| f != 0)
    }

    /// Records a null the runtime returned for a request of `size` bytes:
    /// a refusal unless nothing was asked for.
    fn returned(p: *mut c_void, size: usize) -> *mut c_void {
        if p.is_null() && size > 0 {
            refuse(size);
        }
        p
    }

    unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
        let Some(f) = held(&MALLOC).filter(|_| allowed(size)) else {
            return ptr::null_mut();
        };
        // SAFETY: the runtime's `malloc`, which the import held.
        returned(
            unsafe { std::mem::transmute::<usize, Malloc>(f)(size) },
            size,
        )
    }

    unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
        let Some(total) = count.checked_mul(size) else {
            refuse(usize::MAX);
            return ptr::null_mut();
        };
        let Some(f) = held(&CALLOC).filter(|_| allowed(total)) else {
            return ptr::null_mut();
        };
        // SAFETY: the runtime's `calloc`, which the import held.
        returned(
            unsafe { std::mem::transmute::<usize, Calloc>(f)(count, size) },
            total,
        )
    }

    unsafe extern "C" fn realloc(block: *mut c_void, size: usize) -> *mut c_void {
        let Some(f) = held(&REALLOC).filter(|_| allowed(size)) else {
            return ptr::null_mut();
        };
        // SAFETY: the runtime's `realloc`, which the import held, on a
        // block the runtime allocated. A refusal leaves the block as it was.
        returned(
            unsafe { std::mem::transmute::<usize, Realloc>(f)(block, size) },
            size,
        )
    }

    unsafe extern "C" fn aligned_malloc(size: usize, align: usize) -> *mut c_void {
        let Some(f) = held(&ALIGNED_MALLOC).filter(|_| allowed(size)) else {
            return ptr::null_mut();
        };
        // SAFETY: the runtime's `_aligned_malloc`, which the import held.
        returned(
            unsafe { std::mem::transmute::<usize, AlignedMalloc>(f)(size, align) },
            size,
        )
    }

    unsafe extern "C" fn aligned_realloc(
        block: *mut c_void,
        size: usize,
        align: usize,
    ) -> *mut c_void {
        let Some(f) = held(&ALIGNED_REALLOC).filter(|_| allowed(size)) else {
            return ptr::null_mut();
        };
        // SAFETY: the runtime's `_aligned_realloc`, which the import held,
        // on a block the runtime allocated. A refusal leaves the block as
        // it was.
        returned(
            unsafe { std::mem::transmute::<usize, AlignedRealloc>(f)(block, size, align) },
            size,
        )
    }

    /// Allocates a decoded frame's buffers through FFmpeg's own allocation,
    /// unless the frame is video over the cap at the size the decoder asks
    /// for (its coded size, which FFmpeg allocates): that one is refused,
    /// and recorded, before anything is allocated.
    pub unsafe extern "C" fn get_buffer(
        ctx: *mut ff::AVCodecContext,
        frame: *mut ff::AVFrame,
        flags: c_int,
    ) -> c_int {
        // SAFETY: the context and frame FFmpeg hands a `get_buffer2`.
        unsafe {
            if (*ctx).codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO {
                let (width, height) = ((*frame).width, (*frame).height);
                if !within_cap(width, height) {
                    record(|r| r.oversize.store(true, Ordering::SeqCst));
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "tapestry-media: refused a frame of {width} x {height}"
                    );
                    return averror(ff::EINVAL);
                }
            }
            ff::avcodec_default_get_buffer2(ctx, frame, flags)
        }
    }

    /// What a thread [`begin_thread`] starts gets: FFmpeg's start routine
    /// and its argument, and the record of the thread that started it, with
    /// a count of its `Arc` for the new thread (null when that thread works
    /// for no player).
    struct Started {
        start: Start,
        arg: *mut c_void,
        record: *const Record,
    }

    /// Starts a thread for FFmpeg through the runtime's `_beginthreadex`,
    /// working for the player the starting thread works for: a decoder's
    /// frame and slice threads record for the player whose thread opened
    /// it.
    unsafe extern "C" fn begin_thread(
        security: *mut c_void,
        stack: u32,
        start: Option<Start>,
        arg: *mut c_void,
        flags: u32,
        id: *mut u32,
    ) -> usize {
        let (Some(f), Some(start)) = (held(&BEGIN_THREAD), start) else {
            return 0;
        };
        let layout = Layout::new::<Started>();
        // SAFETY: a layout of non-zero size. The memory comes from Rust's
        // allocator, not the runtime's the hooks stand in front of.
        let started = unsafe { std::alloc::alloc(layout) }.cast::<Started>();
        if started.is_null() {
            return 0;
        }
        let record = CURRENT.with(Cell::get);
        if !record.is_null() {
            // SAFETY: a record `CURRENT` holds a count of: one more, for the
            // new thread.
            unsafe { Arc::increment_strong_count(record) };
        }
        // SAFETY: memory of `Started`'s layout, allocated above; then the
        // runtime's `_beginthreadex`, which the import held, given FFmpeg's
        // arguments but the start routine and its argument.
        let thread = unsafe {
            started.write(Started { start, arg, record });
            std::mem::transmute::<usize, BeginThread>(f)(
                security,
                stack,
                Some(run_started),
                started.cast(),
                flags,
                id,
            )
        };
        if thread == 0 {
            // SAFETY: the `Started` written above, which no thread got.
            unsafe {
                let Started { record, .. } = started.read();
                std::alloc::dealloc(started.cast(), layout);
                if !record.is_null() {
                    drop(Arc::from_raw(record));
                }
            }
        }
        thread
    }

    /// The start routine of each thread [`begin_thread`] starts: FFmpeg's,
    /// run working for the player of the thread that started it.
    unsafe extern "system" fn run_started(started: *mut c_void) -> u32 {
        // SAFETY: the `Started` `begin_thread` wrote for this thread alone,
        // read once and freed.
        let Started { start, arg, record } = unsafe {
            let s = started.cast::<Started>().read();
            std::alloc::dealloc(started.cast(), Layout::new::<Started>());
            s
        };
        let previous = CURRENT.with(|c| c.replace(record));
        // SAFETY: the start routine and argument FFmpeg gave
        // `_beginthreadex`.
        let code = unsafe { start(arg) };
        CURRENT.with(|c| c.set(previous));
        if !record.is_null() {
            // SAFETY: the count `begin_thread` took for this thread.
            unsafe { drop(Arc::from_raw(record)) };
        }
        code
    }

    /// Gives a decoder the pixel cap and the worker's frame allocation.
    ///
    /// # Safety
    ///
    /// `ctx` is a codec context not yet open.
    pub unsafe fn cap(ctx: *mut ff::AVCodecContext) {
        // SAFETY: the caller's.
        unsafe {
            (*ctx).max_pixels = MAX_PIXELS.cast_signed();
            (*ctx).get_buffer2 = Some(get_buffer);
        }
    }

    /// Opens a decoder the format library opens itself, while it probes:
    /// refused unless it decodes a listed codec, and given the pixel cap and
    /// the worker's frame allocation first.
    unsafe extern "C" fn open(
        ctx: *mut ff::AVCodecContext,
        codec: *const ff::AVCodec,
        options: *mut *mut ff::AVDictionary,
    ) -> c_int {
        let Some(f) = held(&OPEN).filter(|_| !ctx.is_null()) else {
            return averror(ff::EPERM);
        };
        // SAFETY: the context and codec FFmpeg passes `avcodec_open2`, the
        // codec null when the context names its own.
        unsafe {
            let codec = if codec.is_null() { (*ctx).codec } else { codec };
            let listed = !codec.is_null()
                && ff::av_codec_is_decoder(codec) != 0
                && CODEC_IDS.contains(&(*codec).id);
            if !listed {
                let id = if codec.is_null() {
                    (*ctx).codec_id
                } else {
                    (*codec).id
                };
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "tapestry-media: refused a decoder for {}",
                    text(ff::avcodec_get_name(id))
                );
                return averror(ff::EPERM);
            }
            cap(ctx);
            std::mem::transmute::<usize, Open>(f)(ctx, codec, options)
        }
    }

    /// Points FFmpeg's DLLs at the hooks: their C runtime's allocators at
    /// the worker's, which refuse an allocation above `max_alloc`, its
    /// thread start at the worker's, and the format library's decoder opens
    /// at the worker's. FFmpeg's own cap is lifted, so a request it would
    /// refuse reaches the worker's allocator.
    pub fn install(max_alloc: usize) -> Result<(), String> {
        CAP.store(max_alloc, Ordering::Relaxed);
        // SAFETY: each returns a static string inside its own library.
        let (util, codec, format, scale) = unsafe {
            (
                ff::av_version_info().cast::<c_void>(),
                ff::avcodec_configuration().cast::<c_void>(),
                ff::avformat_configuration().cast::<c_void>(),
                ff::swscale_configuration().cast::<c_void>(),
            )
        };
        let runtime: [(&CStr, &AtomicUsize, *const c_void); 6] = [
            (c"malloc", &MALLOC, malloc as Malloc as *const c_void),
            (c"calloc", &CALLOC, calloc as Calloc as *const c_void),
            (c"realloc", &REALLOC, realloc as Realloc as *const c_void),
            (
                c"_aligned_malloc",
                &ALIGNED_MALLOC,
                aligned_malloc as AlignedMalloc as *const c_void,
            ),
            (
                c"_aligned_realloc",
                &ALIGNED_REALLOC,
                aligned_realloc as AlignedRealloc as *const c_void,
            ),
            (
                c"_beginthreadex",
                &BEGIN_THREAD,
                begin_thread as BeginThread as *const c_void,
            ),
        ];
        for library in [util, codec, format, scale] {
            for name in OTHER_STARTS {
                let imported = crate::os::imports(library, name)
                    .map_err(|e| format!("{}: {e}", name.to_string_lossy()))?;
                if imported {
                    return Err(format!(
                        "FFmpeg's DLLs start threads through {}, which the worker does not hook",
                        name.to_string_lossy()
                    ));
                }
            }
            for (name, original, hook) in runtime {
                point(library, name, original, hook)?;
            }
        }
        point(
            format,
            c"avcodec_open2",
            &OPEN,
            open as Open as *const c_void,
        )?;
        // `av_malloc` and `av_realloc` allocate through the two aligned
        // allocators, without which the cap would not hold, and FFmpeg's
        // threads start through `_beginthreadex`, without which their
        // refusals would be recorded for no player.
        for (name, original) in [
            ("_aligned_malloc", &ALIGNED_MALLOC),
            ("_aligned_realloc", &ALIGNED_REALLOC),
            ("_beginthreadex", &BEGIN_THREAD),
            ("avcodec_open2", &OPEN),
        ] {
            if held(original).is_none() {
                return Err(format!("FFmpeg's DLLs import no {name}"));
            }
        }
        // SAFETY: a process-wide setting made before any thread that calls
        // FFmpeg starts.
        unsafe { ff::av_max_alloc(usize::MAX) };
        Ok(())
    }

    /// Points `library`'s imports of `name` at `hook`, keeping what they
    /// held in `original`: the same function in every library.
    fn point(
        library: *const c_void,
        name: &CStr,
        original: &AtomicUsize,
        hook: *const c_void,
    ) -> Result<(), String> {
        let what = name.to_string_lossy();
        // SAFETY: no other thread runs yet, and each hook has the type of
        // the function its name names.
        let held = unsafe { crate::os::redirect_import(library, name, hook) }
            .map_err(|e| format!("{what}: {e}"))?;
        if let Some(held) = held {
            let before = original.load(Ordering::Relaxed);
            if before != 0 && before != held {
                return Err(format!("{what}: FFmpeg's DLLs import more than one"));
            }
            original.store(held, Ordering::Relaxed);
        }
        Ok(())
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

/// What an input's I/O context reads through: the file, and the bytes and
/// seeks left to the probe or the seek under way.
struct Budgeted {
    file: std::fs::File,
    /// What a read lands in before it is copied to FFmpeg's buffer, which
    /// FFmpeg may hand uninitialized.
    scratch: Box<[u8]>,
    /// The bytes and seeks left, `None` while the player plays on.
    left: Option<(u64, u32)>,
    /// A read or a seek was refused for the budget.
    spent: bool,
}

/// Reads the next bytes of the file for FFmpeg, charged to the budget.
unsafe extern "C" fn read(opaque: *mut c_void, buf: *mut u8, size: c_int) -> c_int {
    let Ok(mut len) = usize::try_from(size) else {
        return averror(ff::EINVAL);
    };
    if opaque.is_null() || buf.is_null() {
        return averror(ff::EINVAL);
    }
    // SAFETY: `opaque` is the source this module boxed for this context,
    // alive until the context is freed and reached only through it while
    // FFmpeg runs.
    let source = unsafe { &mut *opaque.cast::<Budgeted>() };
    if let Some((bytes, _)) = source.left {
        if bytes == 0 {
            source.spent = true;
            return SPENT;
        }
        len = len.min(usize::try_from(bytes).unwrap_or(usize::MAX));
    }
    // FFmpeg's buffer may be uninitialized, which no slice may cover: the
    // file is read into the source's own bytes, then copied.
    let take = len.min(source.scratch.len());
    let Some(scratch) = source.scratch.get_mut(..take) else {
        return averror(ff::EINVAL);
    };
    match source.file.read(scratch) {
        Ok(0) => ff::AVERROR_EOF,
        Ok(n) => {
            // SAFETY: FFmpeg hands a buffer of `size` bytes; `n` is at most
            // `take`, which is at most `size`, and the scratch is the
            // source's own.
            unsafe { ptr::copy_nonoverlapping(scratch.as_ptr(), buf, n) };
            if let Some((bytes, _)) = &mut source.left {
                *bytes = bytes.saturating_sub(n as u64);
            }
            c_int::try_from(n).unwrap_or(size)
        }
        Err(_) => averror(ff::EIO),
    }
}

/// Moves in the file for FFmpeg, or gives its size, charged to the budget.
unsafe extern "C" fn seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    if opaque.is_null() {
        return i64::from(averror(ff::EINVAL));
    }
    // SAFETY: as in `read`.
    let source = unsafe { &mut *opaque.cast::<Budgeted>() };
    if let Some((_, seeks)) = &mut source.left {
        if *seeks == 0 {
            source.spent = true;
            return i64::from(SPENT);
        }
        *seeks -= 1;
    }
    seek_in(&mut source.file, offset, whence)
}

/// Moves in `file` as FFmpeg asks a seek callback to, or gives its size,
/// read from its metadata without moving: one call moves in the file once
/// at most.
fn seek_in(file: &mut std::fs::File, offset: i64, whence: c_int) -> i64 {
    let at = if whence & ff::AVSEEK_SIZE != 0 {
        file.metadata()
            .ok()
            .and_then(|m| i64::try_from(m.len()).ok())
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

/// Sets the string options `pairs` on an FFmpeg context before it opens,
/// each checked: FFmpeg's error for the first it could not set, a copy of
/// its value refused among them. An open given a dictionary instead goes
/// on without what it could not copy of it.
///
/// # Safety
///
/// `context` is an allocated `AVFormatContext` or `AVCodecContext`.
unsafe fn set_options(context: *mut c_void, pairs: &[(&CStr, &CStr)]) -> Result<(), c_int> {
    for (key, value) in pairs {
        // SAFETY: the caller's context, whose class has these options, and
        // NUL-terminated strings, which FFmpeg copies.
        let r = unsafe { ff::av_opt_set(context, key.as_ptr(), value.as_ptr(), 0) };
        if r < 0 {
            return Err(r);
        }
    }
    Ok(())
}

/// Says that an allocation of `size` bytes was refused.
fn refused_allocation(size: usize) {
    let _ = writeln!(
        std::io::stderr().lock(),
        "tapestry-media: an allocation of {size} bytes was refused"
    );
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
    /// What FFmpeg reads through `pb`: a box of this input's, which the
    /// context reaches through its pointer, freed after the context.
    source: *mut Budgeted,
    stream: c_int,
    time_base: ff::AVRational,
    start: i64,
    width: c_int,
    height: c_int,
    /// The demuxer has given its last packet and the decoder is draining.
    drained: bool,
    /// What this input's player records, on its thread and on FFmpeg's:
    /// the last field, given back after the drop has freed the contexts and
    /// stopped FFmpeg's threads.
    owner: hooks::Owner,
}

impl Input {
    /// Opens and probes `file`: a listed container holding a video of a
    /// listed codec within the pixel cap, whose decoder opens, all read
    /// within the probe's budget.
    ///
    /// # Errors
    ///
    /// Why the file is refused, with FFmpeg's error (0 when there is
    /// none). Whatever FFmpeg made of them, a frame the probe met over the
    /// cap refuses the file for its size, an allocation refused while it
    /// probed for its decoder (with `ENOMEM`), and a probe past its budget
    /// for its reading (with [`SPENT`]).
    pub fn open(file: std::fs::File, io: IoOpen) -> Result<(Self, Probe), (Refusal, c_int)> {
        // Before any call into FFmpeg: this thread, and each thread FFmpeg
        // starts for this input, records for this input's record alone.
        let owner = hooks::Owner::adopt();
        let source = Box::new(Budgeted {
            file,
            scratch: vec![0; IO_BUFFER.unsigned_abs() as usize].into_boxed_slice(),
            left: Some(BUDGET),
            spent: false,
        });
        let mut input = Self {
            fc: ptr::null_mut(),
            pb: ptr::null_mut(),
            dec: ptr::null_mut(),
            packet: ptr::null_mut(),
            frame: ptr::null_mut(),
            scaled: ptr::null_mut(),
            sws: ptr::null_mut(),
            source: Box::into_raw(source),
            stream: -1,
            time_base: ff::AVRational { num: 0, den: 1 },
            start: 0,
            width: 0,
            height: 0,
            drained: false,
            owner,
        };
        // SAFETY: each call gets the contexts this function allocated (or
        // FFmpeg's documented null), checked before use; `input` frees
        // whatever exists when it drops, on every return.
        let probe = unsafe { input.probe(io) };
        let spent = input.source().spent;
        input.budget(None);
        let record = input.owner.record();
        if record.oversize() {
            return Err((Refusal::Size, 0));
        }
        if let Some(size) = record.refused() {
            refused_allocation(size);
            return Err((Refusal::Decoder, averror(ff::ENOMEM)));
        }
        if spent {
            return Err((Refusal::Read, SPENT));
        }
        Ok((input, probe?))
    }

    /// The source FFmpeg reads through.
    fn source(&mut self) -> &mut Budgeted {
        // SAFETY: the box this input made, alive until it drops; FFmpeg
        // reaches it only while one of this input's calls into FFmpeg runs,
        // and none runs while this borrow lives.
        unsafe { &mut *self.source }
    }

    /// Sets the bytes and seeks left to FFmpeg's reads, `None` for no
    /// budget, and forgets a budget spent before.
    fn budget(&mut self, left: Option<(u64, u32)>) {
        let source = self.source();
        source.left = left;
        source.spent = false;
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
            self.pb = ff::avio_alloc_context(
                buf,
                IO_BUFFER,
                0,
                self.source.cast::<c_void>(),
                Some(read),
                None,
                Some(seek),
            );
            if self.pb.is_null() {
                ff::av_free(buf.cast());
                return Err(no_memory);
            }
            (*self.fc).pb = self.pb;
            (*self.fc).flags |= ff::AVFMT_FLAG_CUSTOM_IO;
            if io == IoOpen::Refuse {
                (*self.fc).io_open = Some(refuse_open);
            }
            let lists = [
                [
                    (c"format_whitelist", FORMATS),
                    (c"protocol_whitelist", PROTOCOLS),
                ]
                .as_slice(),
                codec_list(),
            ]
            .concat();
            set_options(self.fc.cast(), &lists).map_err(|r| (Refusal::Decoder, r))?;
            let r = ff::avformat_open_input(
                &raw mut self.fc,
                c"".as_ptr(),
                ptr::null(),
                ptr::null_mut(),
            );
            if r < 0 {
                let why = if r == averror(ff::EIO) {
                    Refusal::Read
                } else {
                    Refusal::Container
                };
                return Err((why, r));
            }

            // Every decoder FFmpeg opens while it probes passes the worker's
            // open, which gives it the pixel cap: a stream's that a demuxer
            // without a header (MPEG-PS) adds as it reads, and one FFmpeg
            // opens again when a stream's parameters change, as well.
            let r = ff::avformat_find_stream_info(self.fc, ptr::null_mut());
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
            (*self.dec).pkt_timebase = (*st).time_base;
            hooks::cap(self.dec);
            set_options(self.dec.cast(), codec_list()).map_err(|r| (Refusal::Decoder, r))?;
            let r = ff::avcodec_open2(self.dec, decoder, ptr::null_mut());
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

    /// Decodes the next frame into `out`, I420 at the probed size: its
    /// stamp, in µs from the stream's start, or `None` at the end. A frame
    /// with no time is decoded but not returned.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, an allocation made for the player is
    /// refused, a frame over the cap among them (one FFmpeg went on from as
    /// well), or the decoder fails; a damaged packet FFmpeg skips is not an
    /// error.
    pub fn next(&mut self, out: &mut [u8]) -> Result<Option<i64>, (Failure, c_int)> {
        let next = self.next_unchecked(out);
        // A refusal made while the frame was decoded and converted, or while
        // the decoder drained, fails the player before the frame or the end
        // is taken.
        self.check()?;
        next
    }

    /// [`Input::next`] before its last look for a refusal.
    fn next_unchecked(&mut self, out: &mut [u8]) -> Result<Option<i64>, (Failure, c_int)> {
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
    /// from a second further back, up to three times, all within one seek's
    /// budget. `None` when no frame at or after the target exists.
    ///
    /// # Errors
    ///
    /// As [`Input::next`], when the demuxer cannot seek, or when the seek
    /// reads past its budget (for reading, with [`SPENT`]).
    pub fn seek(
        &mut self,
        target: i64,
        out: &mut [u8],
    ) -> Result<Option<Landed>, (Failure, c_int)> {
        self.budget(Some(BUDGET));
        let landed = self.seek_within_budget(target, out);
        // As in `next`, before the landing or the end is taken; a seek past
        // its budget fails for its reading.
        let checked = self.check();
        self.budget(None);
        checked?;
        landed
    }

    /// [`Input::seek`]'s seeks and decoding.
    fn seek_within_budget(
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
            self.check()?;
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

    /// Fails the player when an allocation made for it was refused since
    /// the last look, on its thread or on one FFmpeg started for it, or a
    /// frame over the cap, FFmpeg's error or not, or when the seek under way
    /// has spent its budget.
    fn check(&mut self) -> Result<(), (Failure, c_int)> {
        let record = self.owner.record();
        if let Some(size) = record.refused() {
            refused_allocation(size);
            return Err((Failure::Memory, averror(ff::ENOMEM)));
        }
        if record.oversize() {
            // The error the frame allocation gave, whether the decoder
            // passed it on or went on from it.
            return Err((Failure::Decode, averror(ff::EINVAL)));
        }
        if self.source().spent {
            return Err((Failure::Read, SPENT));
        }
        Ok(())
    }

    /// Decodes the next frame into `self.frame`: whether there was one.
    fn decode(&mut self) -> Result<bool, (Failure, c_int)> {
        let mut damaged = 0;
        loop {
            self.check()?;
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
                if r == ff::AVERROR_INVALIDDATA && self.drained {
                    // A damaged frame among those the decoder held when the
                    // file ended: the ones after it still come.
                    damaged += 1;
                    if damaged > DAMAGED_AT_END {
                        return Ok(false);
                    }
                    continue;
                }
                if r != averror(ff::EAGAIN) && r != ff::AVERROR_INVALIDDATA {
                    return Err((Failure::Decode, r));
                }
                if self.drained {
                    return Ok(false);
                }
                let r = ff::av_read_frame(self.fc, self.packet);
                // A refusal the demuxer went on from, as if the block it
                // could not allocate were damaged or the file had ended,
                // fails the player here.
                self.check()?;
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
                        self.check()?;
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
            // The context that read through the source is gone.
            if !self.source.is_null() {
                drop(Box::from_raw(self.source));
                self.source = ptr::null_mut();
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
        file: Box::into_raw(Box::new(file)),
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
    /// The clip's file, which the output context writes through: a box of
    /// this encoding's, freed after the context.
    file: *mut std::fs::File,
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
            let opaque = self.file.cast::<c_void>();
            self.pb = ff::avio_alloc_context(
                buf,
                IO_BUFFER,
                1,
                opaque,
                None,
                Some(write),
                Some(seek_file),
            );
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
        // SAFETY: this encoding's file, which the output context reaches
        // only while one of its calls runs, and none runs now.
        unsafe { &mut *self.file }
            .flush()
            .map_err(|e| e.to_string())
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
            // The context that wrote through the file is gone.
            if !self.file.is_null() {
                drop(Box::from_raw(self.file));
                self.file = ptr::null_mut();
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

/// Moves in the clip's file for FFmpeg, or gives its size.
#[cfg(feature = "dev-tools")]
unsafe extern "C" fn seek_file(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    if opaque.is_null() {
        return i64::from(averror(ff::EINVAL));
    }
    // SAFETY: `opaque` is the file this module boxed for this context,
    // alive until the context is freed and reached only through it while
    // FFmpeg runs.
    seek_in(
        unsafe { &mut *opaque.cast::<std::fs::File>() },
        offset,
        whence,
    )
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
