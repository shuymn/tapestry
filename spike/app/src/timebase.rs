//! QPC time base (design D3: QPC→µs via i128 multiply-then-divide).

use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

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

    pub fn now_us(self) -> i64 {
        self.to_us(qpc())
    }
}
