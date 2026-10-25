//! Dropping the Windows input thread's handle. One test per process: Raw
//! Input has one keyboard registration per process.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use input::{Capture, InputThread, QpcClock, SourceConfig, SourceSet};

/// Sets its flag when dropped, which happens when the thread's hook is.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn dropping_the_input_thread_waits_for_it() {
    let sources = SourceSet::new(SourceConfig {
        max_devices: 9,
        max_sources: 3,
        max_held: 7,
        queue_capacity: 32,
        max_arrivals: 5,
    })
    .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(Arc::clone(&dropped));
    let mut blocked = false;
    let thread = InputThread::spawn(QpcClock::start(), sources, Capture::new(), move |_, _| {
        let _keep = &flag;
        if !blocked {
            // The first wake holds the hook until the test releases it.
            blocked = true;
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
    })
    .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let released = Arc::new(AtomicBool::new(false));
    let releaser = {
        let released = Arc::clone(&released);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            released.store(true, Ordering::Release);
            release_tx.send(()).unwrap();
        })
    };
    drop(thread);
    // Drop returned only after the held hook was released and the thread
    // ended, dropping the hook.
    assert!(released.load(Ordering::Acquire));
    assert!(dropped.load(Ordering::Acquire));
    releaser.join().unwrap();
}
