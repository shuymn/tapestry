//! Windows' effective power mode (§3.5), read without elevation through
//! `PowerRegisterForEffectivePowerModeNotifications`: the callback reports
//! the current mode on registration and every change after it. `measure`
//! keeps a watch over the run; `report` keeps the modes in force during
//! play (`spike_core::conditions::power_modes_during`).

use crate::timebase::qpc;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::System::Power::{
    EFFECTIVE_POWER_MODE, EFFECTIVE_POWER_MODE_V2, PowerRegisterForEffectivePowerModeNotifications,
    PowerUnregisterFromEffectivePowerModeNotifications,
};

pub const POWER_MODES: &str = "private/power_modes.csv";

/// (QPC, mode) of every notification since the watch started. One watch
/// at a time (the callback has no per-watch state).
static CHANGES: Mutex<Vec<(i64, i32)>> = Mutex::new(Vec::new());

unsafe extern "system" fn on_change(mode: EFFECTIVE_POWER_MODE, _: *const c_void) {
    if let Ok(mut v) = CHANGES.lock() {
        v.push((qpc(), mode.0));
    }
}

pub struct Watch(*mut c_void);

impl Watch {
    pub fn start() -> Result<Self, String> {
        if let Ok(mut v) = CHANGES.lock() {
            v.clear();
        }
        let mut handle = std::ptr::null_mut();
        // SAFETY: a static callback without context; the handle is
        // unregistered on drop.
        unsafe {
            PowerRegisterForEffectivePowerModeNotifications(
                EFFECTIVE_POWER_MODE_V2,
                Some(on_change),
                None,
                &mut handle,
            )
        }
        .map_err(|e| format!("effective power mode: {e}"))?;
        Ok(Self(handle))
    }

    pub fn changes(&self) -> Vec<(i64, i32)> {
        CHANGES.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: the handle came from the registration above.
        let _ = unsafe { PowerUnregisterFromEffectivePowerModeNotifications(self.0) };
    }
}

/// The effective power mode now (the registration's first notification).
pub fn current() -> Option<i32> {
    let w = Watch::start().ok()?;
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if let Some(&(_, m)) = w.changes().first() {
            return Some(m);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}
