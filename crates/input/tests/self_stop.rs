//! Stopping the Windows input thread from its own hook. A separate test
//! binary: Raw Input has one keyboard registration per process.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use input::{Capture, InputError, InputThread, QpcClock, SourceConfig, SourceSet};

/// Sets its flag when dropped, which happens when the thread's hook is.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn stopping_from_the_input_thread_detaches_it() {
    let sources = SourceSet::new(SourceConfig {
        max_devices: 6,
        max_sources: 2,
        max_held: 4,
        queue_capacity: 16,
        max_arrivals: 3,
    })
    .unwrap();
    let (handle_tx, handle_rx) = mpsc::channel::<InputThread>();
    let (result_tx, result_rx) = mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(Arc::clone(&dropped));
    let thread = InputThread::spawn(QpcClock::start(), sources, Capture::new(), move |_, _| {
        let _keep = &flag;
        if let Ok(own) = handle_rx.try_recv() {
            // Joining itself here would never return.
            let stopped = own.stop();
            result_tx
                .send(matches!(stopped, Err(InputError::StopFromWorker)))
                .unwrap();
        }
    })
    .unwrap();
    handle_tx.send(thread).unwrap();
    let stopped_with_error = result_rx.recv_timeout(Duration::from_secs(10));
    assert_eq!(stopped_with_error, Ok(true), "the hook did not return");
    // The detached thread ends after that wake and drops its hook.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dropped.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "the detached thread did not end");
        std::thread::sleep(Duration::from_millis(1));
    }
}
