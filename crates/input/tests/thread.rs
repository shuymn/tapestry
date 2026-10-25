//! The Windows input thread's lifecycle. One test per process: Raw Input
//! has one keyboard registration per process.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use input::{Capture, InputThread, QpcClock, SourceConfig, SourceSet};

/// Sets its flag when dropped, which happens when the thread's hook is.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn dropping_the_input_thread_stops_and_joins_it() {
    let sources = SourceSet::new(SourceConfig {
        max_devices: 9,
        max_sources: 3,
        max_held: 7,
        queue_capacity: 32,
        max_arrivals: 5,
    })
    .unwrap();
    let wakes = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let (hook_wakes, flag) = (Arc::clone(&wakes), DropFlag(Arc::clone(&dropped)));
    let thread = InputThread::spawn(QpcClock::start(), sources, Capture::new(), move |_, _| {
        let _keep = &flag;
        hook_wakes.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while wakes.load(Ordering::Relaxed) < 20 {
        assert!(Instant::now() < deadline, "the thread does not wake");
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(thread);
    // Joined: the hook is gone and no wake follows.
    assert!(dropped.load(Ordering::Acquire));
    let after = wakes.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(wakes.load(Ordering::Relaxed), after);
}
