//! Input and logic threads (design D4) and the play barrier they share
//! with the audio and render threads.
//!
//! - Input: a 1 kHz high-resolution timer tick, MMCSS "Games", Raw Input
//!   registered for the keyboard on a message-only window (pumped, events
//!   counted, otherwise unused), and the synthetic presses of
//!   `spike_core::play`: each tick emits the presses now due, stamped with
//!   the tick's QPC, into a ring to the logic thread and wakes it.
//! - Logic: woken by the input thread, judges each press, records
//!   input → judgment latency (G4), and emits a hit event to the render
//!   thread, a sound command to the audio thread and a snapshot (judged
//!   count) through a triple buffer.
//!
//! Every queue is preallocated; a full queue is counted, never silent (an
//! input or sound-command overflow invalidates the run, V5).

use crate::alloc::{self, Tag};
use crate::timebase::{Clock, HiResTimer, qpc, qpc_freq};
use rtrb::{Consumer, Producer};
use spike_core::play::{Judge, Press};
use spike_core::run::{InputLatency, StreamCount};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, OnceLock};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent,
    WaitForSingleObject,
};
use windows::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_INPUTSINK, RegisterRawInputDevices};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG, PM_REMOVE,
    PeekMessageW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WNDCLASSW,
};
use windows::core::w;

/// Queue capacities. A 1 ms tick carries at most a chord's worth of
/// presses; the render thread drains hits once per frame.
pub const INPUT_RING: usize = 4_096;
pub const SOUND_RING: usize = 4_096;
pub const HIT_RING: usize = 4_096;
/// Input tick (1 kHz).
const TICK_US: i64 = 1_000;

/// The play barrier: every G6 thread registers its tag, finishes its setup
/// (including every allocation it needs in play) and waits here; the play
/// start is fixed when the last one arrives, a countdown later. Allocation
/// counting is armed only over the play window, so no thread can still be
/// in setup when it starts (G6).
pub struct PlaySync {
    barrier: Barrier,
    start: OnceLock<i64>,
    clock: Clock,
    countdown_us: i64,
}

impl PlaySync {
    pub fn new(threads: usize, clock: Clock, countdown_us: i64) -> Self {
        Self {
            barrier: Barrier::new(threads),
            start: OnceLock::new(),
            clock,
            countdown_us,
        }
    }

    /// The play start, once every thread has arrived.
    pub fn start(&self) -> Option<i64> {
        self.start.get().copied()
    }

    /// Waits for every thread, then returns the play start (run µs).
    pub fn ready(&self) -> i64 {
        self.barrier.wait();
        *self
            .start
            .get_or_init(|| self.clock.now_us() + self.countdown_us)
    }
}

/// A synthetic press as the input thread emitted it.
#[derive(Clone, Copy, Debug)]
pub struct PressMsg {
    pub press: Press,
    /// QPC of the input tick that emitted it.
    pub t_qpc: i64,
}

/// A judged press for the render thread's effects.
#[derive(Clone, Copy, Debug)]
pub struct HitMsg {
    /// Press time, µs since play start.
    pub t_us: i64,
    pub lane: u8,
}

/// A sound for the audio thread.
#[derive(Clone, Copy, Debug)]
pub struct SoundCmd {
    pub sound: u16,
    /// QPC of the judgment.
    pub judged_qpc: i64,
}

/// What the render thread shows of the logic state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub judged: u32,
}

/// Auto-reset event that wakes the logic thread.
pub struct Event(HANDLE);

// SAFETY: an event handle may be signaled and waited on from any thread.
unsafe impl Send for Event {}
// SAFETY: as above; the handle is only closed on drop.
unsafe impl Sync for Event {}

impl Event {
    pub fn new() -> Result<Self, String> {
        // SAFETY: plain auto-reset event creation, owned by this value.
        unsafe { CreateEventW(None, false, false, None) }
            .map(Self)
            .map_err(|e| format!("CreateEventW: {e}"))
    }

    pub fn set(&self) {
        // SAFETY: valid event handle.
        let _ = unsafe { SetEvent(self.0) };
    }

    /// Waits up to `ms`.
    pub fn wait(&self, ms: u32) {
        // SAFETY: valid event handle.
        unsafe { WaitForSingleObject(self.0, ms) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: owned handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// MMCSS registration of the calling thread for `task` ("Games", "Pro
/// Audio"); reverted on drop. `None` if the service refused.
pub struct Mmcss(HANDLE);

impl Mmcss {
    pub fn join(task: &windows::core::HSTRING) -> Option<Self> {
        let mut index = 0u32;
        // SAFETY: valid task name and out pointer.
        unsafe { AvSetMmThreadCharacteristicsW(task, &mut index) }
            .ok()
            .map(Self)
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on
        // this thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// The input thread's time source: each call waits for the next tick and
/// returns `(µs since play start, QPC)`, or `None` to stop.
pub trait Ticker {
    fn tick(&mut self) -> Option<(i64, i64)>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InputStats {
    pub emitted: u64,
    pub overflows: u64,
    pub ticks: u64,
    pub raw_input_events: u64,
}

/// Emits every press as its time comes due, until all are emitted or the
/// ticker stops; `on_tick` runs once per tick (the message pump).
pub fn input_loop(
    presses: &[Press],
    ticker: &mut impl Ticker,
    tx: &mut Producer<PressMsg>,
    wake: &Event,
    mut on_tick: impl FnMut(),
) -> InputStats {
    let mut s = InputStats::default();
    let mut next = 0;
    while next < presses.len() {
        let Some((play_us, t_qpc)) = ticker.tick() else {
            break;
        };
        s.ticks += 1;
        let from = next;
        while let Some(&press) = presses.get(next)
            && press.t_us <= play_us
        {
            next += 1;
            if tx.push(PressMsg { press, t_qpc }).is_ok() {
                s.emitted += 1;
            } else {
                s.overflows += 1;
            }
        }
        if next > from {
            wake.set();
        }
        on_tick();
    }
    s
}

pub struct LogicOut {
    pub input: Vec<InputLatency>,
    pub input_records: StreamCount,
    pub judged: u32,
    pub sound_overflows: u64,
    pub hit_overflows: u64,
}

/// Where the logic thread sends its results.
pub struct LogicSinks {
    pub hits: Option<Producer<HitMsg>>,
    pub sounds: Option<Producer<SoundCmd>>,
    pub snapshot: triple_buffer::Input<Snapshot>,
}

/// Judges presses as they arrive until the input thread is done and the
/// queue is empty. `records` is the capacity of the latency log (one per
/// note).
pub fn logic_loop(
    mut judge: Judge,
    rx: &mut Consumer<PressMsg>,
    wake: &Event,
    input_done: &AtomicBool,
    sinks: &mut LogicSinks,
    clock: Clock,
    records: usize,
) -> LogicOut {
    let mut out = LogicOut {
        input: Vec::with_capacity(records),
        input_records: StreamCount::default(),
        judged: 0,
        sound_overflows: 0,
        hit_overflows: 0,
    };
    let freq = qpc_freq();
    loop {
        // Read the flag before draining: a press pushed before it was set
        // is then still drained below.
        let done = input_done.load(Ordering::Acquire);
        while let Ok(m) = rx.pop() {
            let t = qpc();
            out.input_records.produced += 1;
            let rec = InputLatency {
                t_us: clock.to_us(m.t_qpc),
                latency_us: ((t - m.t_qpc) as i128 * 1_000_000 / i128::from(freq)) as i64,
            };
            if out.input.len() < out.input.capacity() {
                out.input.push(rec);
            } else {
                out.input_records.lost += 1;
            }
            if let Some(j) = judge.judge(m.press) {
                if let Some(h) = &mut sinks.hits
                    && h.push(HitMsg {
                        t_us: m.press.t_us,
                        lane: j.lane,
                    })
                    .is_err()
                {
                    out.hit_overflows += 1;
                }
                if let Some(s) = &mut sinks.sounds
                    && s.push(SoundCmd {
                        sound: j.sound,
                        judged_qpc: t,
                    })
                    .is_err()
                {
                    out.sound_overflows += 1;
                }
                sinks.snapshot.write(Snapshot {
                    judged: judge.judged(),
                });
            }
        }
        if done {
            break;
        }
        wake.wait(2);
    }
    out.judged = judge.judged();
    out
}

/// Real-time ticker: 1 ms ticks on a high-resolution timer from the play
/// barrier on, ending at `end_us` or when `stop` is set.
struct TimerTicker {
    timer: HiResTimer,
    clock: Clock,
    play_start: i64,
    next_qpc: i64,
    period_qpc: i64,
    end_us: i64,
    stop: Arc<AtomicBool>,
}

impl Ticker for TimerTicker {
    fn tick(&mut self) -> Option<(i64, i64)> {
        self.timer.wait_until(self.next_qpc);
        let now = qpc();
        // Missed ticks are not made up (no bursts).
        while self.next_qpc <= now {
            self.next_qpc += self.period_qpc;
        }
        let play_us = self.clock.to_us(now) - self.play_start;
        (!self.stop.load(Ordering::Relaxed) && play_us <= self.end_us).then_some((play_us, now))
    }
}

/// Message-only window with keyboard Raw Input registered (RIDEV_INPUTSINK:
/// received without focus). Messages are pumped each tick and WM_INPUT is
/// counted; the synthetic presses stand in for the keys.
struct RawInput(HWND);

unsafe extern "system" fn raw_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: forwarding the arguments the system passed.
    unsafe { DefWindowProcW(h, m, w, l) }
}

impl RawInput {
    fn new() -> Result<Self, String> {
        let class = w!("tapestry-spike-raw-input");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(raw_wndproc),
            lpszClassName: class,
            ..Default::default()
        };
        // SAFETY: the class name and window procedure are static.
        unsafe { RegisterClassW(&wc) };
        // SAFETY: message-only window of the class registered above.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .map_err(|e| format!("raw input window: {e}"))?;
        let dev = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        // SAFETY: one valid device description.
        unsafe { RegisterRawInputDevices(&[dev], size_of::<RAWINPUTDEVICE>() as u32) }
            .map_err(|e| format!("RegisterRawInputDevices: {e}"))?;
        Ok(Self(hwnd))
    }

    /// Pumps pending messages; returns the WM_INPUT count.
    fn pump(&self) -> u64 {
        let mut n = 0;
        let mut msg = MSG::default();
        // SAFETY: pumping this thread's own window's messages.
        while unsafe { PeekMessageW(&mut msg, Some(self.0), 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_INPUT {
                n += 1;
            }
            // SAFETY: a message just retrieved.
            unsafe { DispatchMessageW(&msg) };
        }
        n
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        // SAFETY: the window belongs to this thread.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

pub struct InputThread {
    pub handle: JoinHandle<Result<InputStats, String>>,
}

/// Spawns the input thread. It waits at `sync`, then ticks until every
/// press is emitted, `end_us` passes or `stop` is set, and finally sets
/// `done`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_input(
    presses: Vec<Press>,
    mut tx: Producer<PressMsg>,
    wake: Arc<Event>,
    done: Arc<AtomicBool>,
    sync: Arc<PlaySync>,
    clock: Clock,
    end_us: i64,
    stop: Arc<AtomicBool>,
) -> Result<InputThread, String> {
    let handle = std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            alloc::tag_thread(Tag::Input);
            let r = (|| {
                let _mmcss = Mmcss::join(&windows::core::HSTRING::from("Games"));
                let raw = RawInput::new()?;
                let timer = HiResTimer::new()?;
                let period_qpc = qpc_freq() * TICK_US / 1_000_000;
                let play_start = sync.ready();
                let mut ticker = TimerTicker {
                    timer,
                    clock,
                    play_start,
                    next_qpc: qpc(),
                    period_qpc,
                    end_us,
                    stop,
                };
                let mut raw_events = 0;
                let mut s = input_loop(&presses, &mut ticker, &mut tx, &wake, || {
                    raw_events += raw.pump();
                });
                s.raw_input_events = raw_events;
                Ok(s)
            })();
            done.store(true, Ordering::Release);
            wake.set();
            r
        })
        .map_err(|e| format!("spawn input thread: {e}"))?;
    Ok(InputThread { handle })
}

/// Spawns the logic thread; it waits at `sync`, then judges until the
/// input thread is done.
#[allow(clippy::too_many_arguments)]
pub fn spawn_logic(
    judge: Judge,
    mut rx: Consumer<PressMsg>,
    wake: Arc<Event>,
    input_done: Arc<AtomicBool>,
    mut sinks: LogicSinks,
    sync: Arc<PlaySync>,
    clock: Clock,
    records: usize,
) -> Result<JoinHandle<LogicOut>, String> {
    std::thread::Builder::new()
        .name("logic".into())
        .spawn(move || {
            alloc::tag_thread(Tag::Logic);
            sync.ready();
            logic_loop(
                judge,
                &mut rx,
                &wake,
                &input_done,
                &mut sinks,
                clock,
                records,
            )
        })
        .map_err(|e| format!("spawn logic thread: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;
    use spike_core::chart::{Chart, DEFAULT_SEED};
    use spike_core::play::Play;

    /// Virtual 1 ms ticks without waiting.
    struct FastTicker {
        t: i64,
        end: i64,
    }

    impl Ticker for FastTicker {
        fn tick(&mut self) -> Option<(i64, i64)> {
            self.t += TICK_US;
            (self.t <= self.end).then(|| (self.t, qpc()))
        }
    }

    // Design §5 P3: the threadless `advance_to` and the threaded input →
    // logic run give the same judgment count.
    #[test]
    fn threaded_run_judges_as_many_presses_as_advance_to() {
        let chart = Chart::generate(DEFAULT_SEED);
        let play_us = 150_000_000 + 30_000_000;
        let mut threadless = Play::new(&chart, play_us);
        let mut expected = 0u32;
        threadless.advance_to(i64::MAX, |_| expected += 1);

        let (presses, judge) = Play::new(&chart, play_us).split();
        let notes = presses.len();
        // Rings large enough that the virtual-time producer cannot outrun
        // the consumer into an overflow.
        let (mut tx, mut rx) = RingBuffer::<PressMsg>::new(notes);
        let (hits_tx, mut hits_rx) = RingBuffer::<HitMsg>::new(notes);
        let (snap_in, mut snap_out) = triple_buffer::triple_buffer(&Snapshot::default());
        let wake = Arc::new(Event::new().unwrap());
        let done = Arc::new(AtomicBool::new(false));
        let clock = Clock::start();
        let logic = {
            let (wake, done) = (wake.clone(), done.clone());
            std::thread::spawn(move || {
                let mut sinks = LogicSinks {
                    hits: Some(hits_tx),
                    sounds: None,
                    snapshot: snap_in,
                };
                logic_loop(judge, &mut rx, &wake, &done, &mut sinks, clock, notes)
            })
        };
        let mut ticker = FastTicker {
            t: -100_000,
            end: play_us + 100_000,
        };
        let stats = input_loop(&presses, &mut ticker, &mut tx, &wake, || {});
        done.store(true, Ordering::Release);
        wake.set();
        let out = logic.join().unwrap();
        assert_eq!(stats.emitted as usize, notes);
        assert_eq!(stats.overflows, 0);
        assert_eq!(out.judged, expected);
        assert_eq!(expected as usize, notes);
        // One latency record per press, and one hit per judgment.
        assert_eq!(out.input.len(), notes);
        assert_eq!(out.input_records.produced as usize, notes);
        assert_eq!(hits_rx.slots(), notes);
        assert_eq!(snap_out.read().judged, expected);
        assert!(hits_rx.pop().is_ok());
    }

    #[test]
    fn a_full_input_ring_is_counted() {
        let chart = Chart::generate(DEFAULT_SEED);
        let (presses, _) = Play::new(&chart, 10_000_000).split();
        let (mut tx, _rx) = RingBuffer::<PressMsg>::new(8);
        let wake = Event::new().unwrap();
        let mut ticker = FastTicker {
            t: -100_000,
            end: 20_000_000,
        };
        let s = input_loop(&presses, &mut ticker, &mut tx, &wake, || {});
        assert_eq!(s.emitted, 8);
        assert_eq!(s.overflows as usize, presses.len() - 8);
    }
}
