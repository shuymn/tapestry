//! BGA software decode (design D2, D3: ffmpeg-next, LGPL shared FFmpeg,
//! decode thread capped at 2 threads, pool of 8 preallocated frames).
//!
//! The decode thread loops the video, copying each decoded I420 frame into
//! a free pool slot and sending its index. The render thread takes the
//! newest frame due at the predicted display time; older due frames are
//! skipped and counted, and every due frame becomes a P1 `BgaDue` record.

#[cfg(not(feature = "ffmpeg"))]
use spike_core::run::BgaDue;

/// Outcome of taking the frame due at some time.
pub struct Due {
    /// Pool slot to upload, if a new frame is due.
    pub slot: Option<usize>,
}

#[cfg(feature = "ffmpeg")]
mod imp {
    use super::Due;
    use crate::alloc::{self, Scope, Tag, scope};
    use ffmpeg_next as ff;
    use spike_core::run::BgaDue;
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
        /// Presentation time in the looped video, µs.
        pub pts_us: i64,
    }

    pub struct Bga {
        pub width: u32,
        pub height: u32,
        slots: Arc<Vec<Mutex<Slot>>>,
        ready: Receiver<usize>,
        free: SyncSender<usize>,
        stop: Arc<AtomicBool>,
        pub decoded: Arc<AtomicU64>,
        pub decoder_waits: Arc<AtomicU64>,
        /// Received but not yet due.
        pending: Option<usize>,
        pub skipped: u64,
        pub displayed: u64,
    }

    fn probe(path: &Path) -> Result<(u32, u32), String> {
        let ictx = ff::format::input(path).map_err(|e| format!("open BGA: {e}"))?;
        let stream = ictx
            .streams()
            .best(ff::media::Type::Video)
            .ok_or("BGA has no video stream")?;
        let ctx = ff::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| e.to_string())?;
        let dec = ctx.decoder().video().map_err(|e| e.to_string())?;
        Ok((dec.width(), dec.height()))
    }

    impl Bga {
        pub fn open(path: &Path) -> Result<Self, String> {
            ff::init().map_err(|e| e.to_string())?;
            let (width, height) = probe(path)?;
            let (w, h) = (width as usize, height as usize);
            let slots: Arc<Vec<Mutex<Slot>>> = Arc::new(
                (0..POOL)
                    .map(|_| {
                        Mutex::new(Slot {
                            y: vec![0; w * h],
                            u: vec![0; w * h / 4],
                            v: vec![0; w * h / 4],
                            pts_us: 0,
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
            let decoded = Arc::new(AtomicU64::new(0));
            let decoder_waits = Arc::new(AtomicU64::new(0));
            let worker = Worker {
                path: path.to_owned(),
                slots: slots.clone(),
                ready: ready_tx,
                free: free_rx,
                stop: stop.clone(),
                decoded: decoded.clone(),
                decoder_waits: decoder_waits.clone(),
            };
            std::thread::Builder::new()
                .name("bga-decode".into())
                .spawn(move || {
                    alloc::tag_thread(Tag::Other);
                    if let Err(e) = worker.run() {
                        eprintln!("bga-decode: {e}");
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(Self {
                width,
                height,
                slots,
                ready,
                free,
                stop,
                decoded,
                decoder_waits,
                pending: None,
                skipped: 0,
                displayed: 0,
            })
        }

        pub fn slot(&self, i: usize) -> std::sync::MutexGuard<'_, Slot> {
            self.slots[i].lock().expect("BGA slot")
        }

        pub fn release(&self, i: usize) {
            let _ = self.free.send(i);
        }

        fn pts(&self, i: usize) -> i64 {
            self.slot(i).pts_us
        }

        /// Takes the newest decoded frame with `pts <= due_us`. Superseded
        /// due frames are skipped. With `wait`, blocks (up to 2 s per frame)
        /// until the decoder has passed `due_us`, as the offscreen smoke's
        /// virtual clock requires. `base_us` maps video time to run time for
        /// the `BgaDue` records.
        pub fn take_due(
            &mut self,
            due_us: i64,
            wait: bool,
            base_us: i64,
            records: &mut Vec<BgaDue>,
        ) -> Due {
            let mut chosen: Option<usize> = None;
            loop {
                let next = match self.pending.take() {
                    Some(i) => Some(i),
                    None if wait => self.ready.recv_timeout(Duration::from_secs(2)).ok(),
                    None => self.ready.try_recv().ok(),
                };
                let Some(i) = next else { break };
                if self.pts(i) > due_us {
                    self.pending = Some(i);
                    break;
                }
                if let Some(old) = chosen.replace(i) {
                    self.skipped += 1;
                    push(records, base_us + self.pts(old), false);
                    self.release(old);
                }
            }
            if let Some(i) = chosen {
                self.displayed += 1;
                push(records, base_us + self.pts(i), true);
            }
            Due { slot: chosen }
        }
    }

    fn push(records: &mut Vec<BgaDue>, due_us: i64, displayed: bool) {
        if records.len() < records.capacity() {
            records.push(BgaDue { due_us, displayed });
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
        slots: Arc<Vec<Mutex<Slot>>>,
        ready: SyncSender<usize>,
        free: Receiver<usize>,
        stop: Arc<AtomicBool>,
        decoded: Arc<AtomicU64>,
        decoder_waits: Arc<AtomicU64>,
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
            // Each pass of the video is offset by the previous pass's length.
            let mut offset = 0i64;
            let mut last = 0i64;
            let mut step = 16_667i64;
            while !self.stop.load(Ordering::Relaxed) {
                let next = scope(Scope::FfmpegDecode, || {
                    ictx.packets()
                        .find(|(s, _)| s.index() == index)
                        .map(|(_, p)| p)
                });
                match next {
                    Some(packet) => {
                        scope(Scope::FfmpegDecode, || decoder.send_packet(&packet))
                            .map_err(|e| e.to_string())?;
                        self.drain(
                            &mut decoder,
                            &mut frame,
                            &to_us,
                            offset,
                            &mut last,
                            &mut step,
                        )?;
                    }
                    None => {
                        // End of the video: drain, then loop from the start.
                        scope(Scope::FfmpegDecode, || decoder.send_eof())
                            .map_err(|e| e.to_string())?;
                        self.drain(
                            &mut decoder,
                            &mut frame,
                            &to_us,
                            offset,
                            &mut last,
                            &mut step,
                        )?;
                        offset = last + step;
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
            offset: i64,
            last: &mut i64,
            step: &mut i64,
        ) -> Result<(), String> {
            while scope(Scope::FfmpegDecode, || decoder.receive_frame(frame)).is_ok() {
                if frame.format() != ff::format::Pixel::YUV420P {
                    return Err(format!(
                        "BGA pixel format {:?}, need yuv420p",
                        frame.format()
                    ));
                }
                let pts = offset + to_us(frame.timestamp().unwrap_or(0));
                if pts > *last {
                    *step = (pts - *last).clamp(1_000, 100_000);
                }
                *last = pts;
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
                    let Slot { y, u, v, pts_us } = &mut *slot;
                    copy_plane(frame.data(0), frame.stride(0), w, h, y);
                    copy_plane(frame.data(1), frame.stride(1), w / 2, h / 2, u);
                    copy_plane(frame.data(2), frame.stride(2), w / 2, h / 2, v);
                    *pts_us = pts;
                }
                self.decoded.fetch_add(1, Ordering::Relaxed);
                if self.ready.send(i).is_err() {
                    return Ok(());
                }
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
    pub decoded: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub decoder_waits: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub skipped: u64,
    pub displayed: u64,
}

#[cfg(not(feature = "ffmpeg"))]
pub struct Slot {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

#[cfg(not(feature = "ffmpeg"))]
impl Bga {
    pub fn open(_: &std::path::Path) -> Result<Self, String> {
        Err("built without the ffmpeg feature".into())
    }
    pub fn take_due(&mut self, _: i64, _: bool, _: i64, _: &mut Vec<BgaDue>) -> Due {
        Due { slot: None }
    }
    pub fn slot(&self, _: usize) -> Box<Slot> {
        unreachable!("no BGA without ffmpeg")
    }
    pub fn release(&self, _: usize) {}
}
