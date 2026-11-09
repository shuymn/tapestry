//! The checked priority calls of speculative work (design.md "Load
//! Pipeline", system friendliness): a speculative worker enters Windows
//! background mode, which lowers its CPU, I/O and memory priority, and a
//! job promoted to the foreground leaves it on its own thread.
//!
//! Every call's result is checked. A worker whose entry fails runs no
//! speculative job (its caller ends it, logged once); a failed leave keeps
//! the job running in the foreground class at background priority. The
//! scan, which is maintenance, keeps its own fallback to the lowest CPU
//! priority instead (`shelf.rs`).

use std::fmt;

use game::coord::Caps;

use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
    THREAD_PRIORITY,
};

/// A refused priority call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorityError(pub String);

impl fmt::Display for PriorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The calling thread's background mode.
pub trait ThreadMode: Send + Sync {
    /// Enters background mode.
    ///
    /// # Errors
    ///
    /// Why the system refused it.
    fn enter_background(&self) -> Result<(), PriorityError>;

    /// Leaves background mode.
    ///
    /// # Errors
    ///
    /// Why the system refused it.
    fn leave_background(&self) -> Result<(), PriorityError>;
}

/// The priority call on the calling thread.
pub type Call = fn(THREAD_PRIORITY) -> windows::core::Result<()>;

/// The adapter: each mode change made through `call` and checked.
#[derive(Clone, Copy, Debug)]
pub struct Checked {
    call: Call,
}

impl Checked {
    /// The system's call.
    pub const SYSTEM: Self = Self { call: set_current };

    /// The adapter with `call` in place of the system's (tests inject a
    /// refusal through it).
    #[cfg(test)]
    pub const fn with(call: Call) -> Self {
        Self { call }
    }
}

impl ThreadMode for Checked {
    fn enter_background(&self) -> Result<(), PriorityError> {
        (self.call)(THREAD_MODE_BACKGROUND_BEGIN).map_err(|e| PriorityError(e.to_string()))
    }

    fn leave_background(&self) -> Result<(), PriorityError> {
        (self.call)(THREAD_MODE_BACKGROUND_END).map_err(|e| PriorityError(e.to_string()))
    }
}

/// A mode that changes nothing and refuses nothing (tests whose threads
/// keep their priority).
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub struct Unchanged;

#[cfg(test)]
impl ThreadMode for Unchanged {
    fn enter_background(&self) -> Result<(), PriorityError> {
        Ok(())
    }

    fn leave_background(&self) -> Result<(), PriorityError> {
        Ok(())
    }
}

fn set_current(priority: THREAD_PRIORITY) -> windows::core::Result<()> {
    // SAFETY: the pseudo handle of the calling thread, always valid.
    unsafe { SetThreadPriority(GetCurrentThread(), priority) }
}

/// What the speculation setting is weighed against: the machine's cores
/// and physical memory.
pub trait Machine {
    /// The cores available to the process.
    fn cores(&self) -> usize;

    /// The physical memory in bytes.
    ///
    /// # Errors
    ///
    /// Why it cannot be read.
    fn memory(&self) -> Result<u64, String>;
}

/// This machine.
#[derive(Clone, Copy, Debug)]
pub struct System;

impl Machine for System {
    fn cores(&self) -> usize {
        std::thread::available_parallelism().map_or(1, usize::from)
    }

    fn memory(&self) -> Result<u64, String> {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).unwrap_or(u32::MAX),
            ..MEMORYSTATUSEX::default()
        };
        // SAFETY: `status` is a valid MEMORYSTATUSEX with its length set.
        unsafe { GlobalMemoryStatusEx(&raw mut status) }.map_err(|e| e.to_string())?;
        Ok(status.ullTotalPhys)
    }
}

/// The run's permit caps from `device.toml`'s `speculation` on `machine`
/// (design.md "Load Pipeline", the setting), and the line to say when the
/// memory the setting leaves to the machine cannot be read.
pub fn run_caps(device: &game::DeviceSettings, machine: &dyn Machine) -> (Caps, Option<String>) {
    let cores = machine.cores();
    let (memory, note) = if device.speculation.is_some() {
        (None, None)
    } else {
        match machine.memory() {
            Ok(m) => (Some(m), None),
            Err(e) => (
                None,
                Some(format!(
                    "speculation: the memory size cannot be read ({e}); speculation is off"
                )),
            ),
        }
    };
    (Caps::from_setting(device.speculation, cores, memory), note)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use game::ChartSource;
    use game::coord::{Caps, Class, Coordinator};
    use game::{DataRoot, DeviceSettings, Settings};
    use windows::Win32::Foundation::E_ACCESSDENIED;
    use windows::Win32::System::Threading::{THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY};

    use super::{Checked, ThreadMode};
    use crate::background::{Job, Queued, Worker};

    /// The system's calls are checked: entering and leaving succeed on a
    /// thread of its own, and a second entry, or a leave outside
    /// background mode, is refused and said so.
    #[test]
    fn the_system_calls_are_checked() {
        std::thread::spawn(|| {
            let mode = Checked::SYSTEM;
            assert!(mode.leave_background().is_err());
            mode.enter_background().unwrap();
            assert!(mode.enter_background().is_err());
            mode.leave_background().unwrap();
        })
        .join()
        .unwrap();
    }

    fn refuse_entry(priority: THREAD_PRIORITY) -> windows::core::Result<()> {
        if priority == THREAD_MODE_BACKGROUND_BEGIN {
            return Err(E_ACCESSDENIED.into());
        }
        super::set_current(priority)
    }

    fn job(name: &str) -> Job {
        Job {
            source: ChartSource::new(PathBuf::from(name), 0, [0; 32]),
            rate: 8_000,
            name: name.to_owned(),
        }
    }

    /// A machine of `cores` cores whose memory reads as `memory`, counting
    /// the reads.
    struct Fake {
        cores: usize,
        memory: Option<u64>,
        reads: AtomicUsize,
    }

    impl super::Machine for Fake {
        fn cores(&self) -> usize {
            self.cores
        }

        fn memory(&self) -> Result<u64, String> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.memory.ok_or_else(|| "refused (test)".to_owned())
        }
    }

    /// The run's caps come from `device.toml` as loaded: `false` turns
    /// speculation off on a large machine, `true` turns it on on three
    /// cores whose memory cannot be read, and the key left out leaves it to
    /// the machine, with one line when its memory cannot be read.
    #[test]
    fn the_run_builds_its_caps_from_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().to_owned());
        let fake = |cores, memory| Fake {
            cores,
            memory,
            reads: AtomicUsize::new(0),
        };
        let caps = |speculation: Option<bool>, machine: &Fake| {
            let device = DeviceSettings {
                speculation,
                ..DeviceSettings::default()
            };
            game::save_settings(&root.device_settings(), &device).unwrap();
            let loaded = Settings::load(&root).unwrap();
            assert_eq!(loaded.device.speculation, speculation);
            super::run_caps(&loaded.device, machine)
        };
        let big = fake(16, Some(64 << 30));
        assert_eq!(caps(Some(false), &big), (Caps::for_machine(0, 16), None));
        let small = fake(3, None);
        assert_eq!(caps(Some(true), &small), (Caps::for_machine(1, 3), None));
        assert_eq!(small.reads.load(Ordering::SeqCst), 0);
        assert_eq!(caps(None, &big), (Caps::for_machine(1, 16), None));
        let unknown = fake(16, None);
        let (got, note) = caps(None, &unknown);
        assert_eq!(got.speculative, 0);
        assert_eq!(unknown.reads.load(Ordering::SeqCst), 1);
        assert_eq!(
            note.as_deref(),
            Some(
                "speculation: the memory size cannot be read (refused (test)); speculation is off"
            )
        );
    }

    /// The system's memory size reads, and is at least 1 GiB on any machine
    /// that runs the tests.
    #[test]
    fn this_machines_memory_reads() {
        assert!(super::Machine::memory(&super::System).unwrap() >= 1 << 30);
    }

    /// A worker whose entry into background mode is refused (through the
    /// seam) runs no speculative job: its thread ends with no measurement
    /// begun, and a job handed to it is not queued.
    #[test]
    fn a_refused_entry_runs_no_speculative_job() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let measure: crate::background::Measure = Box::new(move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            crate::background::Outcome::Stopped
        });
        let worker = Worker::start(
            measure,
            Coordinator::new(Caps::for_machine(1, 16)),
            Arc::new(Checked::with(refuse_entry)),
        )
        .unwrap();
        worker.queue(job("a"));
        assert!(worker.wait_ended(Duration::from_secs(600)));
        assert_eq!(worker.queue(job("b")), Queued::Ended);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(worker.take_results().is_empty());
    }

    /// With the system's calls, a promotion leaves background mode on the
    /// worker's own thread, and the demotion enters it again there while
    /// the measurement goes on: a call made on another thread, or a missed
    /// leave, would refuse the entry (a thread already in background mode)
    /// and end the worker before the next job.
    #[test]
    fn a_promotion_leaves_and_its_demotion_reenters_on_the_workers_thread() {
        use crate::background::tests::{Said, Stepper};
        let coord = Coordinator::new(Caps::for_machine(1, 16));
        let (stepper, measure) = Stepper::new();
        let worker = Worker::start(measure, coord.clone(), Arc::new(Checked::SYSTEM)).unwrap();
        // Declared after the worker, so dropped before it: a failing test
        // closes the step channel before the worker's drop joins its thread.
        let stepper = stepper;
        let held = || {
            (
                coord.held(Class::Speculative),
                coord.held(Class::Foreground),
            )
        };
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        assert_eq!(held(), (1, 0));
        worker.promote(Path::new("a"));
        stepper.step();
        assert_eq!(stepper.said(), Said::Block(1));
        assert_eq!(held(), (0, 1));
        worker.demote(Path::new("a"));
        stepper.step();
        assert_eq!(stepper.said(), Said::Block(2));
        assert_eq!(held(), (1, 0));
        stepper.finish();
        assert!(worker.wait_results(1, Duration::from_secs(600)));
        assert_eq!(worker.queue(job("b")), Queued::Added);
        assert_eq!(stepper.said(), Said::Block(0));
        stepper.finish();
        assert!(worker.wait_results(2, Duration::from_secs(600)));
        let paths: Vec<PathBuf> = worker.take_results().into_iter().map(|f| f.path).collect();
        assert_eq!(paths, vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(held(), (0, 0));
    }
}
