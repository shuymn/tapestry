//! The system's suspend notification (design.md "Handling QPC"): a
//! suspend aborts the play and marks its record incomplete (D13). The
//! callback form needs no window, so it works whatever winit does with its
//! own messages. Without it a suspend would go unnoticed and the record
//! would carry the jumped clock as if nothing happened, so the game refuses
//! to start when the registration fails.

use std::ffi::c_void;
use std::sync::Arc;

use game::Command;
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, WIN32_ERROR};
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
    ///
    /// # Errors
    ///
    /// The system's refusal.
    pub fn register(controls: Arc<Controls>) -> anyhow::Result<Self> {
        Self::register_with(controls, |recipient, handle| {
            // SAFETY: the recipient points to parameters the watch keeps
            // until it unregisters.
            unsafe {
                PowerRegisterSuspendResumeNotification(DEVICE_NOTIFY_CALLBACK, recipient, handle)
            }
        })
    }

    /// [`register`](Self::register) through `register`, which a test
    /// replaces to make it fail.
    fn register_with(
        controls: Arc<Controls>,
        register: impl FnOnce(HANDLE, *mut *mut c_void) -> WIN32_ERROR,
    ) -> anyhow::Result<Self> {
        let mut context = Box::new((controls, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS::default()));
        let ptr: *const Arc<Controls> = &raw const context.0;
        context.1 = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(on_power),
            Context: ptr.cast_mut().cast(),
        };
        let mut handle = std::ptr::null_mut();
        let recipient = HANDLE((&raw const context.1).cast_mut().cast());
        let status = register(recipient, &mut handle);
        if status != ERROR_SUCCESS {
            anyhow::bail!(
                "the suspend notification cannot be registered (error {}), so a suspend could \
                 not end the play",
                status.0
            );
        }
        Ok(Self {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use game::Command;
    use input::QpcClock;
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows::Win32::UI::WindowsAndMessaging::{PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND};

    use super::{SuspendWatch, on_power};
    use crate::controls::Controls;

    #[test]
    fn a_refused_registration_is_an_error() {
        let controls = Arc::new(Controls::new(QpcClock::start()));
        let refused = SuspendWatch::register_with(controls, |_, _| ERROR_ACCESS_DENIED);
        let e = format!("{:#}", refused.err().expect("refused"));
        assert!(e.contains("error 5"), "{e}");
    }

    #[test]
    fn the_registered_callback_sends_suspend_only() {
        let controls = Arc::new(Controls::new(QpcClock::start()));
        let watch = SuspendWatch::register(Arc::clone(&controls)).expect("registration");
        let context = watch._context.1.Context;
        // SAFETY: the context the registration passes to the callback.
        unsafe { on_power(context, PBT_APMRESUMEAUTOMATIC, std::ptr::null()) };
        assert_eq!(controls.take(), None);
        // SAFETY: as above.
        unsafe { on_power(context, PBT_APMSUSPEND, std::ptr::null()) };
        assert_eq!(controls.take(), Some(Command::Suspend));
    }
}
