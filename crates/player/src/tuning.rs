//! The settings keys' values while the game runs (stage1.md PR16): the
//! window thread steps them on F1 to F8, the render thread reads hi-speed
//! and the display offset every frame, and the game thread forwards the
//! volume to the audio thread, takes the input offset into the next play
//! and writes the files when a play ends. Lock-free and allocation-free.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use game::{Adjust, Tuning};

/// The four values, shared.
#[derive(Debug)]
pub struct LiveTuning {
    visible_time_ms: AtomicU32,
    volume_percent: AtomicU32,
    display_offset_us: AtomicI64,
    input_offset_us: AtomicI64,
    /// Counts the changes, so readers notice one.
    version: AtomicU64,
    /// Whether the end screen shows: only it takes the input offset keys.
    end_screen: AtomicBool,
}

impl LiveTuning {
    /// The values `t`, with no end screen shown.
    pub const fn new(t: Tuning) -> Self {
        Self {
            visible_time_ms: AtomicU32::new(t.visible_time_ms),
            volume_percent: AtomicU32::new(t.volume_percent),
            display_offset_us: AtomicI64::new(t.display_offset_us),
            input_offset_us: AtomicI64::new(t.input_offset_us),
            version: AtomicU64::new(0),
            end_screen: AtomicBool::new(false),
        }
    }

    /// The current values. Only the window thread changes them, one at a
    /// time, so a read between two changes sees each value as it is.
    pub fn get(&self) -> Tuning {
        Tuning {
            visible_time_ms: self.visible_time_ms.load(Ordering::Acquire),
            volume_percent: self.volume_percent.load(Ordering::Acquire),
            display_offset_us: self.display_offset_us.load(Ordering::Acquire),
            input_offset_us: self.input_offset_us.load(Ordering::Acquire),
        }
    }

    /// How many changes there have been.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// One step of a settings key, from the window thread. The input
    /// offset keys work on the end screen only, so a play never changes
    /// its own input offset (D13). Returns whether a value changed.
    pub fn adjust(&self, a: Adjust) -> bool {
        if a.is_input_offset() && !self.end_screen.load(Ordering::Acquire) {
            return false;
        }
        let old = self.get();
        let new = old.adjust(a);
        if new == old {
            return false;
        }
        self.visible_time_ms
            .store(new.visible_time_ms, Ordering::Release);
        self.volume_percent
            .store(new.volume_percent, Ordering::Release);
        self.display_offset_us
            .store(new.display_offset_us, Ordering::Release);
        self.input_offset_us
            .store(new.input_offset_us, Ordering::Release);
        self.version.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// The end screen shows, or a play is being prepared.
    pub fn set_end_screen(&self, shown: bool) {
        self.end_screen.store(shown, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use game::{Adjust, Tuning};

    use super::LiveTuning;

    fn start() -> Tuning {
        Tuning {
            visible_time_ms: 600,
            volume_percent: 100,
            display_offset_us: 0,
            input_offset_us: 2_000,
        }
    }

    #[test]
    fn input_offset_keys_work_on_the_end_screen_only() {
        let t = LiveTuning::new(start());
        assert!(!t.adjust(Adjust::InputOffsetUp));
        assert!(!t.adjust(Adjust::InputOffsetDown));
        assert_eq!((t.get(), t.version()), (start(), 0));
        t.set_end_screen(true);
        assert!(t.adjust(Adjust::InputOffsetUp));
        assert_eq!(t.get().input_offset_us, 3_000);
        assert_eq!(t.version(), 1);
        t.set_end_screen(false);
        assert!(!t.adjust(Adjust::InputOffsetDown));
        assert_eq!(t.get().input_offset_us, 3_000);
    }

    #[test]
    fn a_step_at_a_bound_changes_nothing() {
        let t = LiveTuning::new(start());
        // Volume is at unity already.
        assert!(!t.adjust(Adjust::VolumeUp));
        assert_eq!(t.version(), 0);
        assert!(t.adjust(Adjust::HiSpeedUp));
        assert!(t.adjust(Adjust::DisplayOffsetDown));
        assert_eq!(
            t.get(),
            Tuning {
                visible_time_ms: 575,
                display_offset_us: -1_000,
                ..start()
            }
        );
        assert_eq!(t.version(), 2);
    }
}
