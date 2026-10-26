//! The system's suspend notification (design.md "Handling QPC"): a
//! suspend aborts the play and marks its record incomplete (D13). The
//! callback form needs no window, so it works whatever winit does with its
//! own messages.

use std::ffi::c_void;
use std::sync::Arc;

use game::Command;
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, PowerRegisterSuspendResumeNotification,
    PowerUnregisterSuspendResumeNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_APMSUSPEND};

use crate::controls::Controls;

/// The registration, removed on drop.
pub struct SuspendWatch {
    handle: *mut c_void,
    /// The callback's context and parameters, kept alive while registered.
    _context: Box<(Arc<Controls>, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS)>,
}

unsafe extern "system" fn on_power(
    context: *const c_void,
    kind: u32,
    _setting: *const c_void,
) -> u32 {
    if kind == PBT_APMSUSPEND {
        // SAFETY: the context is the `Arc<Controls>` the registration keeps
        // alive until it is removed.
        let controls = unsafe { &*context.cast::<Arc<Controls>>() };
        controls.command(Command::Suspend);
    }
    ERROR_SUCCESS.0
}

impl SuspendWatch {
    /// Sends [`Command::Suspend`] to `controls` when the system suspends.
    /// `None` if the registration is refused; play then goes on without it.
    pub fn register(controls: Arc<Controls>) -> Option<Self> {
        let mut context = Box::new((controls, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS::default()));
        let ptr: *const Arc<Controls> = &raw const context.0;
        context.1 = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(on_power),
            Context: ptr.cast_mut().cast(),
        };
        let mut handle = std::ptr::null_mut();
        let recipient = HANDLE((&raw const context.1).cast_mut().cast());
        // SAFETY: the parameters and their context live in the box, which
        // the watch keeps until it unregisters.
        let status = unsafe {
            PowerRegisterSuspendResumeNotification(DEVICE_NOTIFY_CALLBACK, recipient, &mut handle)
        };
        (status == ERROR_SUCCESS).then_some(Self {
            handle,
            _context: context,
        })
    }
}

impl Drop for SuspendWatch {
    fn drop(&mut self) {
        // SAFETY: the handle came from the registration above.
        let _ =
            unsafe { PowerUnregisterSuspendResumeNotification(HPOWERNOTIFY(self.handle as isize)) };
    }
}
