//! QPC time base (design D3: QPC→µs via i128 multiply-then-divide).

use std::sync::OnceLock;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};

fn freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: writes one i64; cannot fail on supported Windows versions.
        unsafe { QueryPerformanceFrequency(&mut f) }.expect("QueryPerformanceFrequency");
        f
    })
}

pub fn qpc() -> i64 {
    let mut c = 0i64;
    // SAFETY: writes one i64; cannot fail on supported Windows versions.
    unsafe { QueryPerformanceCounter(&mut c) }.expect("QueryPerformanceCounter");
    c
}

pub fn qpc_freq() -> i64 {
    freq()
}

/// Run clock: µs since the run origin.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    origin: i64,
}

impl Clock {
    pub fn start() -> Self {
        Self { origin: qpc() }
    }

    pub fn to_us(self, qpc: i64) -> i64 {
        spike_core::time::qpc_to_us(qpc, self.origin, freq())
    }

    /// Raw QPC of the origin (private: never exported).
    pub fn origin_qpc(self) -> i64 {
        self.origin
    }

    pub fn now_us(self) -> i64 {
        self.to_us(qpc())
    }
}

/// High-resolution waitable timer (frame cap, input thread tick); no
/// `timeBeginPeriod` (design D3 coexistence rules).
pub struct HiResTimer(HANDLE);

impl HiResTimer {
    pub fn new() -> Result<Self, String> {
        // SAFETY: plain timer creation; the handle is owned by this value.
        let h = unsafe {
            CreateWaitableTimerExW(
                None,
                windows::core::PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .map_err(|e| format!("CreateWaitableTimerExW: {e}"))?;
        Ok(Self(h))
    }

    /// Sleeps until QPC `deadline` (no spinning).
    pub fn wait_until(&self, deadline: i64) {
        let left = deadline - qpc();
        if left <= 0 {
            return;
        }
        // Relative due time in 100 ns units (negative).
        let due = -((i128::from(left) * 10_000_000 / i128::from(qpc_freq())) as i64);
        // SAFETY: valid timer handle; no completion routine.
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.0, 1000);
            }
        }
    }
}

impl Drop for HiResTimer {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: a waitable timer handle may be used from any thread.
unsafe impl Send for HiResTimer {}
