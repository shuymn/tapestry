//! BGA software decode (design D2, D3: ffmpeg-next, LGPL shared FFmpeg,
//! decode thread capped at 2 threads, pool of 8 preallocated frames).
//!
//! The decode thread loops the video, copying each decoded I420 frame into
//! a free pool slot and sending its index. Frames are numbered by their
//! position in the looped video (pts / frame interval). The render thread
//! asks for the frame due at its predicted display time and gets the newest
//! decoded frame not after it; older ones are released unshown. Which frame
//! each presented frame showed is recorded by the caller, and G7's due
//! records are built from that (`pacing::bga_due_records`), so frames that
//! were never decoded count as missed.

/// Interval when the stream does not state its frame rate (60 fps).
pub const DEFAULT_INTERVAL_US: i64 = 16_667;

/// First frame number of the next pass of the looped video. Passes are
/// `clip_frames` long (from the stream's frame count or duration), so
/// frames missing at the end of a pass keep their numbers and count as
/// missed; only when the clip length is unknown (0) does the next pass
/// follow the last decoded frame (`next`).
#[cfg_attr(not(feature = "ffmpeg"), allow(dead_code))]
pub fn next_pass_base(base: u64, clip_frames: u64, next: u64) -> u64 {
    if clip_frames > 0 {
        base + clip_frames
    } else {
        next
    }
}

#[cfg(feature = "ffmpeg")]
mod imp {
    use crate::alloc::{self, Scope, Tag, scope};
    use crate::timebase::{qpc, qpc_freq};
    use ffmpeg_next as ff;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const POOL: usize = 8;

    pub struct Slot {
        pub y: Vec<u8>,
        pub u: Vec<u8>,
        pub v: Vec<u8>,
        /// Frame number in the looped video.
        pub index: u64,
    }

    pub struct Bga {
        pub width: u32,
        pub height: u32,
        pub interval_us: i64,
        slots: Arc<Vec<Mutex<Slot>>>,
        ready: Receiver<usize>,
        free: SyncSender<usize>,
        stop: Arc<AtomicBool>,
        error: Arc<Mutex<Option<String>>>,
        pub decoded: Arc<AtomicU64>,
        /// Times the decoder found every slot in use and waited. With the
        /// decoder ahead of the display this is normal backpressure, not
        /// the D4 "exhausted pool": a due frame that could not be decoded
        /// in time shows up as a missed due record instead.
        pub decoder_waits: Arc<AtomicU64>,
        /// Decode time per frame (send + receive), µs, excluding the copy
        /// and any wait for a free slot (§3.2).
        pub decode_us: Arc<Mutex<Vec<u32>>>,
        /// Received but not yet due.
        pending: Option<usize>,
        /// Decoded frames released without being uploaded.
        pub superseded: u64,
    }

    /// Width, height, frame interval (µs) and clip length in frames (0 if
    /// the stream states neither its frame count nor its duration).
    fn probe(path: &Path) -> Result<(u32, u32, i64, u64), String> {
        let ictx = ff::format::input(path).map_err(|e| format!("open BGA: {e}"))?;
        let stream = ictx
            .streams()
            .best(ff::media::Type::Video)
            .ok_or("BGA has no video stream")?;
        let rate = stream.avg_frame_rate();
        let interval = if rate.numerator() > 0 {
            1_000_000 * i64::from(rate.denominator()) / i64::from(rate.numerator())
        } else {
            super::DEFAULT_INTERVAL_US
        };
        let tb = stream.time_base();
        let clip_frames = if stream.frames() > 0 {
            stream.frames() as u64
        } else if stream.duration() > 0 && tb.denominator() > 0 {
            let us = i128::from(stream.duration()) * 1_000_000 * i128::from(tb.numerator())
                / i128::from(tb.denominator());
            ((us + i128::from(interval) / 2) / i128::from(interval)) as u64
        } else {
            0
        };
        let ctx = ff::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| e.to_string())?;
        let dec = ctx.decoder().video().map_err(|e| e.to_string())?;
        Ok((dec.width(), dec.height(), interval, clip_frames))
    }

    impl Bga {
        pub fn open(path: &Path) -> Result<Self, String> {
            ff::init().map_err(|e| e.to_string())?;
            let (width, height, interval_us, clip_frames) = probe(path)?;
            let (w, h) = (width as usize, height as usize);
            let slots: Arc<Vec<Mutex<Slot>>> = Arc::new(
                (0..POOL)
                    .map(|_| {
                        Mutex::new(Slot {
                            y: vec![0; w * h],
                            u: vec![0; w * h / 4],
                            v: vec![0; w * h / 4],
                            index: 0,
                        })
                    })
                    .collect(),
            );
            let (ready_tx, ready) = sync_channel(POOL);
            let (free, free_rx) = sync_channel(POOL);
            for i in 0..POOL {
                free.send(i).expect("free list has room");
            }
            let stop = Arc::new(AtomicBool::new(false));
            let error = Arc::new(Mutex::new(None));
            let decoded = Arc::new(AtomicU64::new(0));
            let decoder_waits = Arc::new(AtomicU64::new(0));
            let decode_us = Arc::new(Mutex::new(Vec::with_capacity(1 << 16)));
            let worker = Worker {
                path: path.to_owned(),
                interval_us,
                clip_frames,
                slots: slots.clone(),
                ready: ready_tx,
                free: free_rx,
                stop: stop.clone(),
                decoded: decoded.clone(),
                decoder_waits: decoder_waits.clone(),
                decode_us: decode_us.clone(),
            };
            let err = error.clone();
            std::thread::Builder::new()
                .name("bga-decode".into())
                .spawn(move || {
                    alloc::tag_thread(Tag::Other);
                    if let Err(e) = worker.run() {
                        *err.lock().expect("BGA error") = Some(e);
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(Self {
                width,
                height,
                interval_us,
                slots,
                ready,
                free,
                stop,
                error,
                decoded,
                decoder_waits,
                decode_us,
                pending: None,
                superseded: 0,
            })
        }

        pub fn slot(&self, i: usize) -> std::sync::MutexGuard<'_, Slot> {
            self.slots[i].lock().expect("BGA slot")
        }

        pub fn release(&self, i: usize) {
            let _ = self.free.send(i);
        }

        fn failed(&self) -> Option<String> {
            self.error.lock().ok().and_then(|e| e.clone())
        }

        /// The newest decoded frame with index `<= due`, as a pool slot the
        /// caller uploads and releases. With `wait`, blocks (up to 2 s per
        /// frame) until the decoder has passed `due`; a decoder error, or a
        /// stall while waiting, is an error.
        pub fn take(&mut self, due: u64, wait: bool) -> Result<Option<usize>, String> {
            if let Some(e) = self.failed() {
                return Err(format!("BGA decoder failed: {e}"));
            }
            let mut chosen: Option<usize> = None;
            loop {
                let next = match self.pending.take() {
                    Some(i) => Some(i),
                    None if wait => match self.ready.recv_timeout(Duration::from_secs(2)) {
                        Ok(i) => Some(i),
                        Err(_) => {
                            return Err(self.failed().map_or_else(
                                || format!("BGA decoder stalled before frame {due}"),
                                |e| format!("BGA decoder failed: {e}"),
                            ));
                        }
                    },
                    None => self.ready.try_recv().ok(),
                };
                let Some(i) = next else { break };
                if self.slot(i).index > due {
                    self.pending = Some(i);
                    break;
                }
                if let Some(old) = chosen.replace(i) {
                    self.superseded += 1;
                    self.release(old);
                }
            }
            Ok(chosen)
        }
    }

    impl Drop for Bga {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // Unblock a decoder waiting for a free slot.
            while let Ok(i) = self.ready.try_recv() {
                let _ = self.free.try_send(i);
            }
        }
    }

    struct Worker {
        path: PathBuf,
        interval_us: i64,
        clip_frames: u64,
        slots: Arc<Vec<Mutex<Slot>>>,
        ready: SyncSender<usize>,
        free: Receiver<usize>,
        stop: Arc<AtomicBool>,
        decoded: Arc<AtomicU64>,
        decoder_waits: Arc<AtomicU64>,
        decode_us: Arc<Mutex<Vec<u32>>>,
    }

    impl Worker {
        fn run(self) -> Result<(), String> {
            let mut ictx = ff::format::input(&self.path).map_err(|e| e.to_string())?;
            let stream = ictx
                .streams()
                .best(ff::media::Type::Video)
                .ok_or("no video stream")?;
            let index = stream.index();
            let tb = stream.time_base();
            let mut ctx = ff::codec::context::Context::from_parameters(stream.parameters())
                .map_err(|e| e.to_string())?;
            ctx.set_threading(ff::codec::threading::Config {
                kind: ff::codec::threading::Type::Frame,
                count: 2,
            });
            let mut decoder = ctx.decoder().video().map_err(|e| e.to_string())?;
            let mut frame = ff::frame::Video::empty();
            let to_us = |ts: i64| {
                (i128::from(ts) * 1_000_000 * i128::from(tb.numerator())
                    / i128::from(tb.denominator().max(1))) as i64
            };
            // Frame numbers continue across passes of the looped video.
            let mut base = 0u64;
            let mut next = 0u64;
            while !self.stop.load(Ordering::Relaxed) {
                let t0 = qpc();
                let packet = scope(Scope::FfmpegDecode, || {
                    ictx.packets()
                        .find(|(s, _)| s.index() == index)
                        .map(|(_, p)| p)
                });
                match packet {
                    Some(packet) => {
                        scope(Scope::FfmpegDecode, || decoder.send_packet(&packet))
                            .map_err(|e| e.to_string())?;
                        self.drain(&mut decoder, &mut frame, &to_us, base, &mut next, t0)?;
                    }
                    None => {
                        // End of the video: drain, then loop from the start.
                        scope(Scope::FfmpegDecode, || decoder.send_eof())
                            .map_err(|e| e.to_string())?;
                        self.drain(&mut decoder, &mut frame, &to_us, base, &mut next, t0)?;
                        if next == base {
                            return Err("BGA pass decoded no frames".into());
                        }
                        base = super::next_pass_base(base, self.clip_frames, next);
                        scope(Scope::FfmpegDecode, || {
                            decoder.flush();
                            ictx.seek(0, ..)
                        })
                        .map_err(|e| e.to_string())?;
                    }
                }
            }
            Ok(())
        }

        fn drain(
            &self,
            decoder: &mut ff::decoder::Video,
            frame: &mut ff::frame::Video,
            to_us: &dyn Fn(i64) -> i64,
            base: u64,
            next: &mut u64,
            mut t0: i64,
        ) -> Result<(), String> {
            while scope(Scope::FfmpegDecode, || decoder.receive_frame(frame)).is_ok() {
                let decode = ((qpc() - t0) * 1_000_000 / qpc_freq()) as u32;
                if let Ok(mut d) = self.decode_us.lock()
                    && d.len() < d.capacity()
                {
                    d.push(decode);
                }
                if frame.format() != ff::format::Pixel::YUV420P {
                    return Err(format!(
                        "BGA pixel format {:?}, need yuv420p",
                        frame.format()
                    ));
                }
                let pts = to_us(frame.timestamp().unwrap_or(0));
                let index = base + (pts + self.interval_us / 2).div_euclid(self.interval_us) as u64;
                *next = (*next).max(index + 1);
                let i = match self.free.try_recv() {
                    Ok(i) => i,
                    Err(_) => {
                        self.decoder_waits.fetch_add(1, Ordering::Relaxed);
                        match self.free.recv() {
                            Ok(i) => i,
                            Err(_) => return Ok(()),
                        }
                    }
                };
                if self.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                {
                    let mut slot = self.slots[i].lock().expect("BGA slot");
                    let (w, h) = (frame.width() as usize, frame.height() as usize);
                    let Slot { y, u, v, index: si } = &mut *slot;
                    copy_plane(frame.data(0), frame.stride(0), w, h, y);
                    copy_plane(frame.data(1), frame.stride(1), w / 2, h / 2, u);
                    copy_plane(frame.data(2), frame.stride(2), w / 2, h / 2, v);
                    *si = index;
                }
                self.decoded.fetch_add(1, Ordering::Relaxed);
                if self.ready.send(i).is_err() {
                    return Ok(());
                }
                // The next frame's decode time starts after this hand-off.
                t0 = qpc();
            }
            Ok(())
        }
    }

    fn copy_plane(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut [u8]) {
        for row in 0..h {
            dst[row * w..(row + 1) * w].copy_from_slice(&src[row * stride..row * stride + w]);
        }
    }
}

#[cfg(feature = "ffmpeg")]
pub use imp::Bga;

/// Stand-in when built without FFmpeg (`--no-default-features`).
#[cfg(not(feature = "ffmpeg"))]
pub struct Bga {
    pub width: u32,
    pub height: u32,
    pub interval_us: i64,
    pub decoded: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub decoder_waits: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub decode_us: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    pub superseded: u64,
}

#[cfg(not(feature = "ffmpeg"))]
pub struct Slot {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub index: u64,
}

#[cfg(not(feature = "ffmpeg"))]
impl Bga {
    pub fn open(_: &std::path::Path) -> Result<Self, String> {
        Err("built without the ffmpeg feature; run without --bga".into())
    }
    pub fn take(&mut self, _: u64, _: bool) -> Result<Option<usize>, String> {
        Ok(None)
    }
    pub fn slot(&self, _: usize) -> Box<Slot> {
        unreachable!("no BGA without ffmpeg")
    }
    pub fn release(&self, _: usize) {}
}

#[cfg(test)]
mod tests {
    use super::next_pass_base;

    #[test]
    fn frames_lost_before_the_end_of_a_pass_keep_their_numbers() {
        // A 180-frame clip whose frames 178 and 179 were never decoded:
        // the next pass still starts at 180.
        assert_eq!(next_pass_base(0, 180, 178), 180);
        assert_eq!(next_pass_base(180, 180, 359), 360);
        // Unknown clip length: follow the decoded frames.
        assert_eq!(next_pass_base(0, 0, 178), 178);
    }
}
